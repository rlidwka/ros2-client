use std::marker::PhantomData;

use serde::{Deserialize, Serialize};
#[allow(unused_imports)]
use log::{debug, error, info, warn};
use bytes::{BufMut, Bytes, BytesMut};
use rustdds::{
  dds::{ReadError, ReadResult, WriteError, WriteResult},
  rpc::*,
  serialization::{deserialize_from_cdr_with_decoder_and_rep_id, deserialize_from_cdr_with_rep_id},
  *,
};

use crate::{message::Message, message_info::MessageInfo};
use super::{request_id, RmwRequestId, ServiceMapping};

// trait Wrapper is for interfacing to Service-specific (De)SerializerAdapter.
// These adapters are essentially pass-through, and do no actual serialization.
// (De)Serialization is done in Wrappers, because they know which ServiceMapping
// to apply, unlike (De)Serializer or their adapters. ServiceMapping must be
// known in order to decode or generate the wire representation.
pub(super) trait Wrapper {
  fn from_bytes_and_ri(input_bytes: &[u8], encoding: RepresentationIdentifier) -> Self;
  fn bytes(&self) -> Bytes;
}

pub(crate) struct RequestWrapper<R> {
  serialized_message: Bytes,
  encoding: RepresentationIdentifier,
  phantom: PhantomData<R>,
}

impl<R> Wrapper for RequestWrapper<R> {
  fn from_bytes_and_ri(input_bytes: &[u8], encoding: RepresentationIdentifier) -> Self {
    RequestWrapper {
      serialized_message: Bytes::copy_from_slice(input_bytes), // cloning here
      encoding,
      phantom: PhantomData,
    }
  }
  fn bytes(&self) -> Bytes {
    self.serialized_message.clone()
  }
}

impl<'de, R> RequestWrapper<R>
where
  R: Deserialize<'de>,
{
  // This will decode the RequestWrapper to Request in Server
  pub(super) fn unwrap(
    &self,
    service_mapping: ServiceMapping,
    message_info: &MessageInfo,
  ) -> ReadResult<(RmwRequestId, R)> {
    self.unwrap_seed(service_mapping, message_info, PhantomData)
  }
}

impl<'de, R> RequestWrapper<R> {
  // This will decode the RequestWrapper to Request in Server
  pub(super) fn unwrap_seed<S>(
    &self,
    service_mapping: ServiceMapping,
    message_info: &MessageInfo,
    seed: S,
  ) -> ReadResult<(RmwRequestId, R)>
  where
    S: serde::de::DeserializeSeed<'de, Value = R>,
  {
    match service_mapping {
      ServiceMapping::Basic => {
        // 1. decode "RequestHeader" and
        // 2. decode Request
        let mut bytes = self.serialized_message.clone(); // ref copy only
        let (header, header_size) =
          deserialize_from_cdr_with_rep_id::<BasicRequestHeader>(&bytes, self.encoding)?;
        if bytes.len() < header_size {
          read_error_deserialization!("Service request too short")
        } else {
          let _header_bytes = bytes.split_off(header_size);
          let (request, _request_bytes) =
            deserialize_from_cdr_with_decoder_and_rep_id(&bytes, self.encoding, seed)?;
          Ok((RmwRequestId::from(header.request_id), request))
        }
      }
      ServiceMapping::Enhanced => {
        // Enhanced mode does not use any header in the DDS payload.
        // Therefore, we use a wrapper that is identical to the payload.
        let (request, _request_bytes) = deserialize_from_cdr_with_decoder_and_rep_id(
          &self.serialized_message,
          self.encoding,
          seed,
        )?;
        let mut rmw_req_id = RmwRequestId::from(
          message_info.related_sample_identity()
            .unwrap_or_else(|| {
              // ServiceMapping::Enhanced is supposed to contain related sample identity as
              // inline QoS parameter.
              //
              // Use the identity of the incoming request as a default, if there was no
              // related sample identity specified in inline QoS.
              let backup_identity = message_info.sample_identity();
              warn!("RequestWrapper::unwrap: related_sample_identity missing. Using sample_identity = {backup_identity:?}");
              backup_identity
            })
        );

        // Logic added for eProsima FastDDS compatibility:
        //
        // If the SequenceNumber in related_sample_identity (presumable from inline QoS)
        // is SEQUENCENUMBER_UNKNOWN, then it cannot refer to a real and valid DATA submessage.
        // We patch the situation by using the actual SequenceNumber of the Request DATA submessage.
        //
        // Maybe FastDDS just forgets to set the field in RELATED_SAMPLE_IDENTITY inline QoS parameter?
        if rmw_req_id.sequence_number == SequenceNumber::UNKNOWN {
          rmw_req_id.sequence_number = message_info.sample_identity().sequence_number;
        }

        Ok((rmw_req_id, request))
      }
      ServiceMapping::Cyclone => cyclone_unwrap_seed(
        self.serialized_message.clone(),
        message_info.writer_guid(),
        self.encoding,
        seed,
      ),
    }
  }
}

impl<R: Serialize> RequestWrapper<R> {
  // Client creates new RequestWrappers from Requests
  pub(super) fn new(
    service_mapping: ServiceMapping,
    r_id: RmwRequestId,
    encoding: RepresentationIdentifier,
    request: R,
  ) -> WriteResult<Self, ()> {
    let mut ser_buffer = BytesMut::with_capacity(std::mem::size_of::<R>() * 3 / 2).writer();

    // First, write header
    match service_mapping {
      ServiceMapping::Basic => {
        let basic_header = BasicRequestHeader::new(r_id.into());
        serialization::to_writer_with_rep_id(&mut ser_buffer, &basic_header, encoding)?;
      }
      ServiceMapping::Enhanced => {
        // This mapping does not use any header, so nothing to do here.
      }
      ServiceMapping::Cyclone => {
        let cyclone_header = CycloneHeader::new(r_id);
        serialization::to_writer_with_rep_id(&mut ser_buffer, &cyclone_header, encoding)?;
      }
    }
    // Second, write request
    serialization::to_writer_with_rep_id(&mut ser_buffer, &request, encoding)?;
    // Ok, assemble result
    Ok(RequestWrapper {
      serialized_message: ser_buffer.into_inner().freeze(),
      encoding,
      phantom: PhantomData,
    })
  }
}

pub(crate) struct ResponseWrapper<R> {
  serialized_message: Bytes,
  encoding: RepresentationIdentifier,
  phantom: PhantomData<R>,
}

impl<R> Wrapper for ResponseWrapper<R> {
  fn from_bytes_and_ri(input_bytes: &[u8], encoding: RepresentationIdentifier) -> Self {
    ResponseWrapper {
      serialized_message: Bytes::copy_from_slice(input_bytes), // cloning here
      encoding,
      phantom: PhantomData,
    }
  }
  fn bytes(&self) -> Bytes {
    self.serialized_message.clone()
  }
}

// impl<'de, R> ResponseWrapper<R>
// where
//   R: Deserialize<'de>,
// {
//   // Client decodes ResponseWrapper to Response
//   // message_info is from Server's response message
//   pub(super) fn unwrap(
//     &self,
//     service_mapping: ServiceMapping,
//     message_info: &MessageInfo,
//     client_guid: GUID,
//   ) -> ReadResult<(RmwRequestId, R)> {
//     self.unwrap_seed(service_mapping, message_info, client_guid, PhantomData)
//   }
// }

impl<'de, R> ResponseWrapper<R> {
  // Client decodes ResponseWrapper to Response
  // message_info is from Server's response message
  pub(super) fn unwrap_seed<S>(
    &self,
    service_mapping: ServiceMapping,
    message_info: &MessageInfo,
    client_guid: GUID,
    seed: S,
  ) -> ReadResult<(RmwRequestId, R)>
  where
    S: serde::de::DeserializeSeed<'de, Value = R>,
  {
    match service_mapping {
      ServiceMapping::Basic => {
        let mut bytes = self.serialized_message.clone(); // ref copy only
        let (header, header_size) =
          deserialize_from_cdr_with_rep_id::<BasicReplyHeader>(&bytes, self.encoding)?;
        if bytes.len() < header_size {
          read_error_deserialization!("Service response too short")
        } else {
          let _header_bytes = bytes.split_off(header_size);
          let (response, _bytes) =
            deserialize_from_cdr_with_decoder_and_rep_id(&bytes, self.encoding, seed)?;
          Ok((RmwRequestId::from(header.related_request_id), response))
        }
      }
      ServiceMapping::Enhanced => {
        // Enhanced mode does not use any header in the DDS payload.
        // Therefore, we use a wrapper that is identical to the payload.
        let (response, _response_bytes) = deserialize_from_cdr_with_decoder_and_rep_id(
          &self.serialized_message,
          self.encoding,
          seed,
        )?;
        let related_sample_identity = match message_info.related_sample_identity() {
          Some(rsi) => rsi,
          None => {
            return read_error_deserialization!("ServiceMapping=Enhanced, but response message did not have related_sample_identity parameter!")
          }
        };
        Ok((RmwRequestId::from(related_sample_identity), response))
      }
      ServiceMapping::Cyclone => {
        // Cyclone constructs the client GUID from two parts
        let mut client_guid_bytes = [0; 16];
        {
          let (first_half, second_half) = client_guid_bytes.split_at_mut(8);

          // This seems a bit odd, but source is
          // https://github.com/ros2/rmw_connextdds/blob/master/rmw_connextdds_common/src/common/rmw_impl.cpp
          // function take_response()
          first_half.copy_from_slice(&client_guid.to_bytes().as_slice()[0..8]);

          // This is received in the wrapper header
          second_half.copy_from_slice(&message_info.writer_guid().to_bytes()[8..16]);
        }
        let client_guid = GUID::from_bytes(client_guid_bytes);

        cyclone_unwrap_seed(
          self.serialized_message.clone(),
          client_guid,
          self.encoding,
          seed,
        )
      }
    }
  }
}

impl<R: Serialize> ResponseWrapper<R> {
  // Server creates new ResponseWrapper from Response
  pub(super) fn new(
    service_mapping: ServiceMapping,
    r_id: RmwRequestId,
    encoding: RepresentationIdentifier,
    response: R,
  ) -> WriteResult<Self, ()> {
    let mut ser_buffer = BytesMut::with_capacity(std::mem::size_of::<R>() * 3 / 2).writer();
    match service_mapping {
      ServiceMapping::Basic => {
        let basic_header = BasicReplyHeader::new(r_id.into());
        serialization::to_writer_with_rep_id(&mut ser_buffer, &basic_header, encoding)?;
      }
      ServiceMapping::Enhanced => {
        // No header, nothing to write here.
      }
      ServiceMapping::Cyclone => {
        let cyclone_header = CycloneHeader::new(r_id);
        serialization::to_writer_with_rep_id(&mut ser_buffer, &cyclone_header, encoding)?;
      }
    }
    serialization::to_writer_with_rep_id(&mut ser_buffer, &response, encoding)?;
    let serialized_message = ser_buffer.into_inner().freeze();
    Ok(ResponseWrapper {
      serialized_message,
      encoding,
      phantom: PhantomData,
    })
  }
}

// Basic mode header is specified in
// RPC over DDS Section "7.5.1.1.1 Common Types"
#[derive(Serialize, Deserialize)]
pub struct BasicRequestHeader {
  // "struct RequestHeader":
  request_id: SampleIdentity,
  instance_name: String, // This is apparently not used: Always sent as empty string.
}
impl BasicRequestHeader {
  fn new(request_id: SampleIdentity) -> Self {
    BasicRequestHeader {
      request_id,
      instance_name: "".to_string(),
    }
  }
}
impl Message for BasicRequestHeader {}

#[derive(Serialize, Deserialize)]
pub struct BasicReplyHeader {
  // "struct ReplyHeader":
  related_request_id: SampleIdentity,
  remote_exception_code: u32, /* It is uncertain if this is ever used. Transmitted as zero
                               * ("REMOTE_EX_OK"). */
}
impl BasicReplyHeader {
  fn new(related_request_id: SampleIdentity) -> Self {
    BasicReplyHeader {
      related_request_id,
      remote_exception_code: 0,
    }
  }
}
impl Message for BasicReplyHeader {}

// Cyclone mode header
//
// This is reverse-engineered from
// https://github.com/ros2/rmw_cyclonedds/blob/master/rmw_cyclonedds_cpp/src/rmw_node.cpp
// https://github.com/ros2/rmw_cyclonedds/blob/master/rmw_cyclonedds_cpp/src/serdata.hpp
// This is a header that Cyclone puts in DDS messages. Same header is used for
// Request and Response.
#[derive(Serialize, Deserialize)]
pub struct CycloneHeader {
  guid_second_half: [u8; 8], // CycloneDDS RMW only sends last 8 bytes of client GUID
  sequence_number_high: i32,
  sequence_number_low: u32,
}
impl CycloneHeader {
  fn new(r_id: RmwRequestId) -> Self {
    let sn = r_id.sequence_number;
    let mut guid_second_half = [0; 8];
    // writer_guid means client GUID (i.e. request writer)
    guid_second_half.copy_from_slice(&r_id.writer_guid.to_bytes()[8..16]);

    CycloneHeader {
      guid_second_half,
      sequence_number_high: sn.high(),
      sequence_number_low: sn.low(),
    }
  }
}
impl Message for CycloneHeader {}

// helper function, because Cyclone Request and Response unwrapping/decoding are
// the same.
fn cyclone_unwrap_seed<'de, R, S>(
  serialized_message: Bytes,
  writer_guid: GUID,
  encoding: RepresentationIdentifier,
  seed: S,
) -> ReadResult<(RmwRequestId, R)>
where
  S: serde::de::DeserializeSeed<'de, Value = R>,
{
  // 1. decode "CycloneHeader" and
  // 2. decode Request/response
  let mut bytes = serialized_message; // ref copy only, to make "mutable"
  let (header, header_size) = deserialize_from_cdr_with_rep_id::<CycloneHeader>(&bytes, encoding)?;
  if bytes.len() < header_size {
    read_error_deserialization!("Service message too short")
  } else {
    let _header_bytes = bytes.split_off(header_size);
    let (response, _response_bytes) =
      deserialize_from_cdr_with_decoder_and_rep_id(&bytes, encoding, seed)?;
    let req_id = RmwRequestId {
      writer_guid, // TODO: This seems to be completely wrong!!!
      // When we are the client, we get half of Client GUID on the CycloneHeader, other half from
      // Client State when we are the server, we get half of Client GUID on the CycloneHeader,
      // other half from writer_guid.
      sequence_number: request_id::SequenceNumber::from_high_low(
        header.sequence_number_high,
        header.sequence_number_low,
      ),
    };
    Ok((req_id, response))
  }
}

pub(super) type SimpleDataReaderR<RW> =
  no_key::SimpleDataReader<RW, ServiceDeserializerAdapter<RW>>;
pub(super) type DataWriterR<RW> = no_key::DataWriter<RW, ServiceSerializerAdapter<RW>>;

pub(super) struct ServiceDeserializerAdapter<RW> {
  phantom: PhantomData<RW>,
}
pub(super) struct ServiceSerializerAdapter<RW> {
  phantom: PhantomData<RW>,
}

impl<RW> ServiceDeserializerAdapter<RW> {
  const REPR_IDS: [RepresentationIdentifier; 2] = [
    RepresentationIdentifier::CDR_BE,
    RepresentationIdentifier::CDR_LE,
  ];
}

impl<RW: Wrapper> no_key::DeserializerAdapter<RW> for ServiceDeserializerAdapter<RW> {
  type Error = ReadError;
  type Decoded = RW;

  fn supported_encodings() -> &'static [RepresentationIdentifier] {
    &Self::REPR_IDS
  }

  fn transform_decoded(decoded: Self::Decoded) -> RW {
    decoded
  }
}

impl<RW: Wrapper> no_key::DefaultDecoder<RW> for ServiceDeserializerAdapter<RW> {
  type Decoder = WrapperDecoder;
  const DECODER: Self::Decoder = WrapperDecoder;
}

#[derive(Clone)]
pub struct WrapperDecoder;

impl<RW> no_key::Decode<RW> for WrapperDecoder
where
  RW: Wrapper,
{
  type Error = ReadError;

  fn decode_bytes(
    self,
    input_bytes: &[u8],
    encoding: RepresentationIdentifier,
  ) -> Result<RW, Self::Error> {
    Ok(RW::from_bytes_and_ri(input_bytes, encoding))
  }
}

impl<RW: Wrapper> no_key::SerializerAdapter<RW> for ServiceSerializerAdapter<RW> {
  type Error = WriteError<()>;
  fn output_encoding() -> RepresentationIdentifier {
    RepresentationIdentifier::CDR_LE
  }

  fn to_bytes(value: &RW) -> WriteResult<Bytes, ()> {
    Ok(value.bytes())
  }
}
