// This file was autogenerated from Opc.Ua.Types.bsd.xml
// DO NOT EDIT THIS FILE

use std::io::{Read, Write};

#[allow(unused_imports)]
use super::super::*;

/// A token representing an anonymous user.
#[derive(Debug, Clone, PartialEq)]
pub struct AnonymousIdentityToken {
    pub policy_id: UAString,
}

impl BinaryEncoder<AnonymousIdentityToken> for AnonymousIdentityToken {
    fn byte_len(&self) -> usize {
        let mut size = 0;
        size += self.policy_id.byte_len();
        size
    }

    #[allow(unused_variables)]
    fn encode<S: Write>(&self, stream: &mut S) -> EncodingResult<usize> {
        let mut size = 0;
        size += self.policy_id.encode(stream)?;
        Ok(size)
    }

    #[allow(unused_variables)]
    fn decode<S: Read>(stream: &mut S) -> EncodingResult<Self> {
        let policy_id = UAString::decode(stream)?;
        Ok(AnonymousIdentityToken {
            policy_id,
        })
    }
}