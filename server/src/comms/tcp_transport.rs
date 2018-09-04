//! The TCP transport module handles receiving and sending of binary data in chunks, handshake,
//! session creation and dispatching of messages via message handler.
//!
//! Internally it uses tokio but the facade is mostly synchronous with the exception of publish
//! responses. i.e. the client is expected to call and wait for a response to their request.
//! Publish requests are sent based on the number of subscriptions and the responses / handling are
//! left to asynchronous event handlers.
use std;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::{Instant, Duration};
use std::sync::{Arc, RwLock, Mutex};

use chrono;
use chrono::Utc;
use futures::{Stream, Future};
use futures::future::{self, loop_fn, Loop};
use futures::sync::mpsc;
use futures::sync::mpsc::{UnboundedSender, UnboundedReceiver, unbounded};
use tokio;
use tokio::net::TcpStream;
use tokio_io::AsyncRead;
use tokio_io::io;
use tokio_io::io::{ReadHalf, WriteHalf};
use tokio_timer::Interval;

use opcua_core::prelude::*;
use opcua_core::comms::message_writer::MessageWriter;
use opcua_core::comms::secure_channel::SecureChannel;
use opcua_types::status_code::StatusCode;
use opcua_types::status_code::StatusCode::*;
use opcua_types::tcp_types::*;

use address_space::types::AddressSpace;
use comms::secure_channel_service::SecureChannelService;
use comms::transport::*;
use constants;
use state::ServerState;
use services::message_handler::MessageHandler;
use session::Session;
use subscriptions::PublishResponseEntry;
use subscriptions::subscription::TickReason;

// TODO these need to go, and use session settings
const RECEIVE_BUFFER_SIZE: usize = 1024 * 64;
const SEND_BUFFER_SIZE: usize = 1024 * 64;
const MAX_MESSAGE_SIZE: usize = 1024 * 64;

macro_rules! connection_finished_test {
    ( $connection:expr ) => {
        {
            let connection = trace_read_lock_unwrap!($connection);
            let finished = connection.is_finished();
            future::ok(!finished)
        }
    }
}

/// This is the thing that handles input and output for the open connection associated with the
/// session.
pub struct TcpTransport {
    // Server state, address space etc.
    server_state: Arc<RwLock<ServerState>>,
    // Session state - open sessions, tokens etc
    session: Arc<RwLock<Session>>,
    /// Secure channel state
    secure_channel: Arc<RwLock<SecureChannel>>,
    /// Address space
    address_space: Arc<RwLock<AddressSpace>>,
    /// The current transport state
    transport_state: TransportState,
    /// Client address
    client_address: Option<SocketAddr>,
    /// Secure channel handler
    secure_channel_service: SecureChannelService,
    /// Message handler
    message_handler: MessageHandler,
    /// Client protocol version set during HELLO
    client_protocol_version: UInt32,
    /// Last decoded sequence number
    last_received_sequence_number: UInt32,
}


struct ReadState {
    /// The associated connection
    pub transport: Arc<RwLock<TcpTransport>>,
    /// The messages buffer
    pub receive_buffer: MessageReader,
    /// Reading portion of socket
    pub reader: ReadHalf<TcpStream>,
    /// Raw bytes in buffer
    pub in_buf: Vec<u8>,
    /// Bytes read in buffer
    pub bytes_read: usize,
    /// Sender of responses
    pub sender: UnboundedSender<(UInt32, SupportedMessage)>,
}

struct WriteState {
    /// The associated connection
    pub transport: Arc<RwLock<TcpTransport>>,
    /// Secure channel state
    pub secure_channel: Arc<RwLock<SecureChannel>>,
    /// Writing portion of socket
    pub writer: Option<WriteHalf<TcpStream>>,
    /// Write buffer (protected since it might be accessed by publish response / event activity)
    pub send_buffer: Arc<Mutex<MessageWriter>>,
}

impl Transport for TcpTransport {
    fn state(&self) -> TransportState {
        self.transport_state
    }

    fn session(&self) -> Arc<RwLock<Session>> {
        self.session.clone()
    }

    fn client_address(&self) -> Option<SocketAddr> {
        self.client_address
    }

    // Terminates the connection and the session
    fn finish(&mut self, status_code: StatusCode) {
        if !self.is_finished() {
            self.transport_state = TransportState::Finished(status_code);
            let mut session = trace_write_lock_unwrap!(self.session);
            session.set_terminated();
        }
    }

    /// Test if the connection is terminated
    fn is_session_terminated(&self) -> bool {
        if let Ok(ref session) = self.session.try_read() {
            session.terminated()
        } else {
            false
        }
    }
}

impl TcpTransport {
    pub fn new(server_state: Arc<RwLock<ServerState>>, session: Arc<RwLock<Session>>, address_space: Arc<RwLock<AddressSpace>>, message_handler: MessageHandler) -> TcpTransport {
        let secure_channel = {
            let session = trace_read_lock_unwrap!(session);
            session.secure_channel.clone()
        };
        let secure_channel_service = SecureChannelService::new();
        TcpTransport {
            server_state,
            session,
            address_space,
            transport_state: TransportState::New,
            client_address: None,
            message_handler,
            secure_channel,
            secure_channel_service,
            client_protocol_version: 0,
            last_received_sequence_number: 0,
        }
    }

    /// This is the entry point for the session. This function is asynchronous - it spawns tokio
    /// tasks to handle the session execution loop so this function will returns immediately.
    pub fn run(connection: Arc<RwLock<TcpTransport>>, socket: TcpStream) {
        // Store the address of the client
        {
            let mut connection = trace_write_lock_unwrap!(connection);
            connection.client_address = Some(socket.peer_addr().unwrap());
            connection.transport_state = TransportState::WaitingHello;
        }
        // Spawn the tasks we need to run
        Self::spawn_looping_task(connection, socket);
    }

    fn read_bytes_task(connection: ReadState) -> impl Future<Item=ReadState, Error=(Arc<RwLock<TcpTransport>>, StatusCode)> {
        // Stuff is taken out of connection state because it is partially consumed by io::read
        let transport_for_err = connection.transport.clone();
        let transport = connection.transport;
        let receive_buffer = connection.receive_buffer;
        let in_buf = connection.in_buf;
        let reader = connection.reader;
        let sender = connection.sender;

        // Read and process bytes from the stream
        io::read(reader, in_buf).map_err(move |err| {
            error!("Transport IO error {:?}", err);
            (transport_for_err, BadCommunicationError)
        }).map(move |(reader, in_buf, bytes_read)| {
            // Build a new connection state
            ReadState {
                transport,
                receive_buffer,
                bytes_read,
                reader,
                in_buf,
                sender,
            }
        })
    }

    fn write_bytes_task(connection: Arc<Mutex<WriteState>>) -> impl Future<Item=Arc<Mutex<WriteState>>, Error=()> {
        let (writer, bytes_to_write, transport) = {
            let mut connection = trace_lock_unwrap!(connection);
            let writer = connection.writer.take();
            let bytes_to_write = {
                let mut send_buffer = trace_lock_unwrap!(connection.send_buffer);
                send_buffer.bytes_to_write()
            };
            let transport = connection.transport.clone();
            (writer, bytes_to_write, transport)
        };
        io::write_all(writer.unwrap(), bytes_to_write).map_err(move |err| {
            error!("Write IO error {:?}", err);
            let mut transport = trace_write_lock_unwrap!(transport);
            transport.finish(BadCommunicationError);
        }).map(move |(writer, _)| {
            // Build a new connection state
            {
                let mut connection = trace_lock_unwrap!(connection);
                connection.writer = Some(writer);
            }
            connection
        })
    }

    fn spawn_looping_task(transport: Arc<RwLock<TcpTransport>>, socket: TcpStream) {
        let session_start_time = Utc::now();
        info!("Session started {}", session_start_time);

        // Spawn the hello timeout task
        Self::spawn_hello_timeout_task(transport.clone(), session_start_time.clone());

        // These should really come from the session
        let (send_buffer_size, receive_buffer_size) = (SEND_BUFFER_SIZE, RECEIVE_BUFFER_SIZE);

        // The reader task will send responses, the writer task will receive responses
        let (tx, rx) = unbounded::<(UInt32, SupportedMessage)>();
        let send_buffer = Arc::new(Mutex::new(MessageWriter::new(send_buffer_size)));
        let (reader, writer) = socket.split();
        let secure_channel = {
            let transport = trace_read_lock_unwrap!(transport);
            transport.secure_channel.clone()
        };

        // Spawn reading task
        // Spawn the subscription processing task
        Self::spawn_subscriptions_task(transport.clone(), tx.clone());
        Self::spawn_reading_loop_task(reader, tx, transport.clone(), receive_buffer_size);
        Self::spawn_writing_loop_task(writer, rx, secure_channel.clone(), transport.clone(), send_buffer);
    }

    fn spawn_writing_loop_task(writer: WriteHalf<TcpStream>, receiver: UnboundedReceiver<(UInt32, SupportedMessage)>, secure_channel: Arc<RwLock<SecureChannel>>, transport: Arc<RwLock<TcpTransport>>, send_buffer: Arc<Mutex<MessageWriter>>) {
        let connection = Arc::new(Mutex::new(WriteState {
            transport: transport.clone(),
            writer: Some(writer),
            send_buffer,
            secure_channel,
        }));

        // The writing task waits for messages that are to be sent
        let looping_task = receiver.map(move |(request_id, response)| {
            (request_id, response, connection.clone())
        }).take_while(move |(_, response, _)| {
            let take = if let SupportedMessage::Invalid(_) = response {
                error!("Writer is terminating because it received an invalid message");
                let mut transport = trace_write_lock_unwrap!(transport);
                transport.finish(BadCommunicationError);
                false
            } else {
                let mut transport = trace_write_lock_unwrap!(transport);
                if transport.is_server_abort() {
                    info!("Writer communication error (abort)");
                    transport.finish(BadCommunicationError);

                    false
                } else if transport.is_finished() {
                    info!("Writer, transport is finished so terminating");
                    false
                } else {
                    true
                }
            };
            future::ok(take)
        }).for_each(move |(request_id, response, connection)| {
            {
                let connection = trace_lock_unwrap!(connection);
                let mut secure_channel = trace_write_lock_unwrap!(connection.secure_channel);
                let mut send_buffer = trace_lock_unwrap!(connection.send_buffer);
                match response {
                    SupportedMessage::AcknowledgeMessage(ack) => {
                        let _ = send_buffer.write_ack(ack);
                    }
                    msg => {
                        let _ = send_buffer.write(request_id, msg, &mut secure_channel);
                    }
                }
            }
            Self::write_bytes_task(connection).and_then(|connection| {
                let finished = {
                    let connection = trace_lock_unwrap!(connection);
                    let transport = trace_read_lock_unwrap!(connection.transport);
                    transport.is_finished()
                };
                if finished {
                    info!("Writer session status is bad is terminating");
                    Err(())
                } else {
                    Ok(connection)
                }
            }).map(|_| ())
        }).map_err(|e| {
            info!("Writer terminating due to error {:?}", e);
        }).map(|_| {
            info!("Writer is finished");
        });

        tokio::spawn(looping_task);
    }

    fn spawn_reading_loop_task(reader: ReadHalf<TcpStream>, sender: UnboundedSender<(UInt32, SupportedMessage)>, transport: Arc<RwLock<TcpTransport>>, receive_buffer_size: usize) {
        // Connection state is maintained for looping through each task
        let connection = ReadState {
            transport: transport.clone(),
            bytes_read: 0,
            reader,
            receive_buffer: MessageReader::new(receive_buffer_size),
            in_buf: vec![0u8; receive_buffer_size],
            sender,
        };

        // 1. Read bytes
        // 2. Store bytes in a buffer
        // 3. Process any chunks (messages) that can be extracted from buffer
        // 4. Terminate if necessary
        // 5. Go to 1
        let looping_task = loop_fn(connection, move |connection| {
            Self::read_bytes_task(connection).and_then(|mut connection| {
                let is_server_abort = {
                    let transport = trace_read_lock_unwrap!(connection.transport);
                    transport.is_server_abort()
                };
                if is_server_abort {
                    info!("Transport is terminating because server has aborted");
                    return Err((connection.transport.clone(), BadCommunicationError));
                }

                let transport_state = {
                    let transport = trace_read_lock_unwrap!(connection.transport);
                    transport.transport_state.clone()
                };

                if connection.bytes_read > 0 {
                    trace!("Read {} bytes", connection.bytes_read);
                    // Handle the incoming message
                    let mut session_status_code = Good;
                    let result = connection.receive_buffer.store_bytes(&connection.in_buf[..connection.bytes_read]);
                    if result.is_err() {
                        session_status_code = result.unwrap_err();
                    } else {
                        let messages = result.unwrap();
                        for message in messages {
                            match transport_state {
                                TransportState::WaitingHello => {
                                    debug!("Processing HELLO");
                                    if let Message::Hello(hello) = message {
                                        let mut transport = trace_write_lock_unwrap!(connection.transport);
                                        let result = transport.process_hello(hello, &mut connection.sender);
                                        if result.is_err() {
                                            session_status_code = result.unwrap_err();
                                        }
                                    } else {
                                        session_status_code = BadCommunicationError;
                                    }
                                }
                                TransportState::ProcessMessages => {
                                    debug!("Processing message");
                                    if let Message::MessageChunk(chunk) = message {
                                        let mut transport = trace_write_lock_unwrap!(connection.transport);
                                        let result = transport.process_chunk(chunk, &mut connection.sender);
                                        if result.is_err() {
                                            session_status_code = result.unwrap_err();
                                        }
                                    } else {
                                        session_status_code = BadCommunicationError;
                                    }
                                }
                                _ => {
                                    error!("Unknown sesion state, aborting");
                                    session_status_code = BadUnexpectedError;
                                }
                            };
                        }
                    }
                    // Update the session status
                    if session_status_code.is_bad() {
                        let mut transport = trace_write_lock_unwrap!(connection.transport);
                        transport.finish(session_status_code);
                    }
                }
                Ok(connection)
            }).and_then(|connection| {
                // Some handlers might wish to send their message and terminate, in which case this is
                // done here.
                let finished = {
                    // Terminate may have been set somewhere
                    let mut transport = trace_write_lock_unwrap!(connection.transport);
                    let terminate = {
                        let session = trace_read_lock_unwrap!(transport.session);
                        session.terminate_session
                    };
                    if terminate {
                        transport.finish(BadConnectionClosed);
                    }
                    // Other session status
                    transport.is_finished()
                };

                // Abort the session?
                if !finished {
                    Ok(Loop::Continue(connection))
                } else {
                    Ok(Loop::Break(()))
                }
            })
        }).map_err(move |(transport, status_code)| {
            info!("An error occurred, terminating the connection");
            {
                let mut transport = trace_write_lock_unwrap!(transport);
                transport.finish(status_code);
            }
        }).map(|_| {
            info!("Reader is finished");
        });

        tokio::spawn(looping_task);
    }

    /// Makes the tokio task that looks for a hello timeout event, i.e. the connection is opened
    /// but no hello is received and we need to drop the session
    fn spawn_hello_timeout_task(transport: Arc<RwLock<TcpTransport>>, session_start_time: chrono::DateTime<Utc>) {
        struct HelloState {
            /// The associated connection
            pub transport: Arc<RwLock<TcpTransport>>,
            /// Session start time
            pub session_start_time: chrono::DateTime<Utc>,
            /// Hello timeout duration, i.e. how long a session is waiting for the hello before it times out
            pub hello_timeout: chrono::Duration,
        }
        let hello_timeout = {
            let hello_timeout = {
                let transport = trace_read_lock_unwrap!(transport);
                let server_state = trace_read_lock_unwrap!(transport.server_state);
                let server_config = trace_read_lock_unwrap!(server_state.config);
                server_config.tcp_config.hello_timeout as i64
            };
            chrono::Duration::seconds(hello_timeout)
        };
        let state = HelloState {
            transport,
            hello_timeout,
            session_start_time: session_start_time.clone(),
        };

        // Clone the connection so the take_while predicate has its own instance
        let transport_for_take_while = state.transport.clone();
        let task = Interval::new(Instant::now(), Duration::from_millis(constants::HELLO_TIMEOUT_POLL_MS))
            .take_while(move |_| {
                // Terminates when session is no longer waiting for a hello or connection is done
                let transport = trace_read_lock_unwrap!(transport_for_take_while);
                let kill_timer = transport.has_received_hello() || transport.is_finished();
                if kill_timer {
                    debug!("Hello timeout timer is being stopped");
                }
                future::ok(!kill_timer)
            })
            .for_each(move |_| {
                // Check if the session has waited in the hello state for more than the hello timeout period
                let transport_state = {
                    let transport = trace_read_lock_unwrap!(state.transport);
                    transport.state()
                };
                if transport_state == TransportState::WaitingHello {
                    // Check if the time elapsed since the session started exceeds the hello timeout
                    let now = Utc::now();
                    if now.signed_duration_since(state.session_start_time.clone()).num_milliseconds() > state.hello_timeout.num_milliseconds() {
                        // Check if the session has waited in the hello state for more than the hello timeout period
                        info!("Session has been waiting for a hello for more than the timeout period and will now close");
                        let mut transport = trace_write_lock_unwrap!(state.transport);
                        transport.finish(BadTimeout);
                    }
                }
                Ok(())
            }).map_err(|_| {}).map(|_| {
            info!("Hello timeout is finished");
        });
        tokio::spawn(task);
    }

    /// Start the subscription timer to service subscriptions
    fn spawn_subscriptions_task(transport: Arc<RwLock<TcpTransport>>, sender: UnboundedSender<(UInt32, SupportedMessage)>) {
        /// Subscription events are passed sent from the monitor task to the receiver
        #[derive(Clone, Debug, PartialEq)]
        enum SubscriptionEvent {
            PublishResponses(VecDeque<PublishResponseEntry>),
        }
        debug!("spawn_subscriptions_task ");

        // Make a channel for subscriptions
        let (subscription_tx, subscription_rx) = mpsc::unbounded::<SubscriptionEvent>();

        // Create the monitoring timer - this monitors for publish requests and ticks the subscriptions
        {
            struct SubscriptionMonitorState {
                /// The associated connection
                pub transport: Arc<RwLock<TcpTransport>>,
            }

            let state = SubscriptionMonitorState {
                transport: transport.clone(),
            };

            // Clone the connection so the take_while predicate has its own instance
            let transport_for_take_while = state.transport.clone();

            // Creates a repeating interval future that checks subscriptions.
            let interval_duration = Duration::from_millis(constants::SUBSCRIPTION_TIMER_RATE_MS);
            let task = Interval::new(Instant::now(), interval_duration)
                .take_while(move |_| {
                    connection_finished_test!(transport_for_take_while)
                })
                .for_each(move |_| {
                    let transport = trace_read_lock_unwrap!(state.transport);
                    let mut session = trace_write_lock_unwrap!(transport.session);

                    let now = Utc::now();

                    // Request queue might contain stale publish requests
                    session.expire_stale_publish_requests(&now);

                    // Process subscriptions
                    {
                        let address_space = trace_read_lock_unwrap!(transport.address_space);
                        let _ = session.tick_subscriptions(&now, &address_space, TickReason::TickTimerFired);
                    }

                    // Check if there are publish responses to send for transmission
                    if let Some(publish_responses) = session.subscriptions.take_publish_responses() {
                        match subscription_tx.unbounded_send(SubscriptionEvent::PublishResponses(publish_responses)) {
                            Err(error) => {
                                error!("Can't send publish responses, err = {}", error);
                            }
                            Ok(_) => {
                                trace!("Sent publish responses to session task");
                            }
                        }
                    }
                    Ok(())
                }).map_err(|_| ()).map(|_| {
                info!("Subscription monitor is finished");
            });
            tokio::spawn(task);
        }

        // Create the receiving task - this takes publish responses and sends them back to the client
        {
            struct SubscriptionReceiverState {
                /// The associated connection
                pub transport: Arc<RwLock<TcpTransport>>,
            }

            let state = SubscriptionReceiverState {
                transport: transport.clone(),
            };

            // Clone the connection so the take_while predicate has its own instance
            let transport_for_take_while = state.transport.clone();

            tokio::spawn(subscription_rx
                .take_while(move |_| {
                    connection_finished_test!(transport_for_take_while)
                })
                .for_each(move |subscription_event| {
                    // Process publish response events
                    match subscription_event {
                        SubscriptionEvent::PublishResponses(publish_responses) => {
                            trace!("Got {} PublishResponse messages to send", publish_responses.len());
                            for publish_response in publish_responses {
                                trace!("<-- Sending a Publish Response{}, {:?}", publish_response.request_id, &publish_response.response);
                                // Messages will be sent by the writing task
                                let _ = sender.unbounded_send((publish_response.request_id, publish_response.response));
                            }
                        }
                    }
                    Ok(())
                }).map(|_| {
                info!("Subscription receiver is finished")
            }));
        }
    }

    /// Test if the connection should abort
    pub fn is_server_abort(&self) -> bool {
        let server_state = trace_read_lock_unwrap!(self.server_state);
        server_state.is_abort()
    }

    fn process_hello(&mut self, hello: HelloMessage, sender: &mut UnboundedSender<(UInt32, SupportedMessage)>) -> std::result::Result<(), StatusCode> {
        let server_protocol_version = 0;

        trace!("Server received HELLO {:?}", hello);
        if !hello.is_endpoint_url_valid() {
            return Err(BadTcpEndpointUrlInvalid);
        }
        if !hello.is_valid_buffer_sizes() {
            error!("HELLO buffer sizes are invalid");
            return Err(BadCommunicationError);
        }

        // Validate protocol version
        if hello.protocol_version > server_protocol_version {
            return Err(BadProtocolVersionUnsupported);
        }

        let client_protocol_version = hello.protocol_version;

        // Send acknowledge
        let mut acknowledge = AcknowledgeMessage {
            message_header: MessageHeader::new(MessageType::Acknowledge),
            protocol_version: server_protocol_version,
            receive_buffer_size: RECEIVE_BUFFER_SIZE as UInt32,
            send_buffer_size: SEND_BUFFER_SIZE as UInt32,
            max_message_size: MAX_MESSAGE_SIZE as UInt32,
            max_chunk_count: MAX_CHUNK_COUNT as UInt32,
        };
        acknowledge.message_header.message_size = acknowledge.byte_len() as UInt32;

        // New state
        self.transport_state = TransportState::ProcessMessages;
        self.client_protocol_version = client_protocol_version;

        debug!("Sending ACK");
        let _ = sender.unbounded_send((0, SupportedMessage::AcknowledgeMessage(acknowledge)));
        Ok(())
    }

    fn turn_received_chunks_into_message(&mut self, chunks: &Vec<MessageChunk>) -> std::result::Result<SupportedMessage, StatusCode> {
        // Validate that all chunks have incrementing sequence numbers and valid chunk types
        let secure_channel = trace_read_lock_unwrap!(self.secure_channel);
        self.last_received_sequence_number = Chunker::validate_chunks(self.last_received_sequence_number + 1, &secure_channel, chunks)?;
        // Now decode
        Chunker::decode(&chunks, &secure_channel, None)
    }

    fn process_chunk(&mut self, chunk: MessageChunk, sender: &mut UnboundedSender<(UInt32, SupportedMessage)>) -> std::result::Result<(), StatusCode> {
        let message_header = chunk.message_header()?;

        if message_header.is_final == MessageIsFinalType::Intermediate {
            panic!("We don't support intermediate chunks yet");
        } else if message_header.is_final == MessageIsFinalType::FinalError {
            info!("Discarding chunk marked in as final error");
            return Ok(());
        }

        // Decrypt / verify chunk if necessary
        let chunk = {
            let mut secure_channel = trace_write_lock_unwrap!(self.secure_channel);
            secure_channel.verify_and_remove_security(&chunk.data)?
        };

        let in_chunks = vec![chunk];
        let chunk_info = {
            let secure_channel = trace_read_lock_unwrap!(self.secure_channel);
            in_chunks[0].chunk_info(&secure_channel)?
        };

        let request_id = chunk_info.sequence_header.request_id;
        let request = self.turn_received_chunks_into_message(&in_chunks)?;

        // Handle the request, and then send the response back to the caller
        let response = match message_header.message_type {
            MessageChunkType::OpenSecureChannel => {
                let mut secure_channel = trace_write_lock_unwrap!(self.secure_channel);
                self.secure_channel_service.open_secure_channel(&mut secure_channel, &chunk_info.security_header, self.client_protocol_version, &request)?
            }
            MessageChunkType::CloseSecureChannel => {
                self.secure_channel_service.close_secure_channel(&request)?
            }
            MessageChunkType::Message => {
                let response = self.message_handler.handle_message(request_id, request)?;
                if response.is_none() {
                    // No response for the message at this time
                    return Ok(());
                }
                response.unwrap()
            }
        };

        // Send the response for transmission
        let _ = sender.unbounded_send((request_id, response));
        Ok(())
    }
}
