//! A gRPC codec for raw bytes: messages are already postcard-encoded.

use bytes::{Buf, BufMut};
use tonic::Status;
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};

/// Passes message bytes through unchanged.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct BytesCodec;

impl Codec for BytesCodec {
    type Encode = Vec<u8>;
    type Decode = Vec<u8>;
    type Encoder = BytesCodec;
    type Decoder = BytesCodec;

    /// Return the encoder.
    fn encoder(&mut self) -> Self::Encoder {
        BytesCodec
    }

    /// Return the decoder.
    fn decoder(&mut self) -> Self::Decoder {
        BytesCodec
    }
}

impl Encoder for BytesCodec {
    type Item = Vec<u8>;
    type Error = Status;

    /// Copy the message into the frame.
    fn encode(&mut self, item: Vec<u8>, dst: &mut EncodeBuf<'_>) -> Result<(), Status> {
        dst.put_slice(&item);
        Ok(())
    }
}

impl Decoder for BytesCodec {
    type Item = Vec<u8>;
    type Error = Status;

    /// Copy the frame out as one message.
    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Vec<u8>>, Status> {
        let mut bytes = vec![0; src.remaining()];
        src.copy_to_slice(&mut bytes);
        Ok(Some(bytes))
    }
}
