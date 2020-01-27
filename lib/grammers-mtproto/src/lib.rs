mod manual_tl;

use std::collections::VecDeque;
use std::io::{self, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use getrandom::getrandom;
use grammers_crypto::{encrypt_data_v2, AuthKey, Side};
use grammers_tl_types::{self as tl, Identifiable, Serializable};

const DEFAULT_COMPRESSION_THRESHOLD: Option<usize> = Some(512);

/// A builder to configure `MTProto` instances.
pub struct MTProtoBuilder {
    compression_threshold: Option<usize>,
}

/// This structure holds everything needed by the Mobile Transport Protocol.
pub struct MTProto {
    /// The authorization key to use to encrypt payload.
    auth_key: AuthKey,

    /// The time offset from the server's time, in seconds.
    time_offset: i32,

    /// The current salt to be used when encrypting payload.
    salt: i64,

    /// The secure, random identifier for this instance.
    client_id: i64,

    /// The current message sequence number.
    sequence: i32,

    /// The ID of the last message.
    last_msg_id: i64,

    /// A queue of messages that are pending from being sent.
    message_queue: VecDeque<manual_tl::Message>,

    /// Identifiers that need to be acknowledged to the server.
    pending_ack: Vec<i64>,

    /// If present, the threshold in bytes at which a message will be
    /// considered large enough to attempt compressing it. Otherwise,
    /// outgoing messages will never be compressed.
    compression_threshold: Option<usize>,
}

#[derive(Copy, Clone, Debug, Hash, PartialEq)]
pub struct MsgId(i64);

pub struct Response {
    pub msg_id: MsgId,
    pub data: Vec<u8>,
}

/// Represents an error that occurs while enqueueing requests.
#[derive(Debug)]
pub enum EnqueueError {
    /// The request payload is too large and cannot possibly be sent.
    /// Telegram would forcibly close the connection if it was ever sent.
    PayloadTooLarge,

    /// Well-formed data must be padded to 4 bytes.
    IncorrectPadding,
}

impl MTProtoBuilder {
    fn new() -> Self {
        Self {
            compression_threshold: DEFAULT_COMPRESSION_THRESHOLD,
        }
    }

    /// Configures the compression threshold for outgoing messages.
    pub fn compression_threshold(mut self, threshold: Option<usize>) -> Self {
        self.compression_threshold = threshold;
        self
    }

    /// Finishes the builder and returns the `MTProto` instance with all
    /// the configuration changes applied.
    pub fn finish(self) -> MTProto {
        let mut result = MTProto::new();
        result.compression_threshold = self.compression_threshold;
        result
    }
}

impl MTProto {
    /// Creates a new instance with default settings.
    pub fn new() -> Self {
        let client_id = {
            let mut buffer = [0u8; 8];
            getrandom(&mut buffer).expect("failed to generate a secure client_id");
            i64::from_le_bytes(buffer)
        };

        Self {
            auth_key: AuthKey::from_bytes([0; 256]),
            time_offset: 0,
            salt: 0,
            client_id,
            sequence: 0,
            last_msg_id: 0,
            message_queue: VecDeque::new(),
            pending_ack: vec![],
            compression_threshold: DEFAULT_COMPRESSION_THRESHOLD,
        }
    }

    /// Returns a builder to configure certain parameters.
    pub fn build() -> MTProtoBuilder {
        MTProtoBuilder::new()
    }

    /// Enqueues a request and returns its associated `msg_id`.
    ///
    /// Once a response arrives and it is decrypted, the caller
    /// is expected to compare the `msg_id` against previously
    /// enqueued requests to determine to which of these the
    /// response belongs.
    ///
    /// If a certain amount of time passes and the enqueued
    /// request has not been sent yet, the message ID will
    /// become stale and Telegram will reject it.
    pub fn enqueue_request(&mut self, mut body: Vec<u8>) -> Result<MsgId, EnqueueError> {
        if body.len() + manual_tl::Message::SIZE_OVERHEAD
            > manual_tl::MessageContainer::MAXIMUM_SIZE
        {
            return Err(EnqueueError::PayloadTooLarge);
        }
        if body.len() % 4 != 0 {
            return Err(EnqueueError::IncorrectPadding);
        }

        // Payload from the outside is always considered to be
        // content-related, which means we can apply compression.
        if let Some(threshold) = self.compression_threshold {
            if body.len() >= threshold {
                let compressed = manual_tl::GzipPacked::new(&body).to_bytes();
                if compressed.len() < body.len() {
                    body = compressed;
                }
            }
        }

        Ok(self.enqueue_body(body, true))
    }

    /// Enqueues a request to be executed by the server sequentially after
    /// another specific request.
    pub fn enqueue_sequential_request(
        &mut self,
        body: Vec<u8>,
        after: &MsgId,
    ) -> Result<MsgId, EnqueueError> {
        self.enqueue_request(
            tl::functions::InvokeAfterMsg {
                msg_id: after.0,
                query: body,
            }
            .to_bytes(),
        )
    }

    fn enqueue_body(&mut self, body: Vec<u8>, content_related: bool) -> MsgId {
        let msg_id = self.get_new_msg_id();
        let seq_no = self.get_seq_no(content_related);
        self.message_queue.push_back(manual_tl::Message {
            msg_id,
            seq_no,
            body,
        });

        MsgId(msg_id)
    }

    /// Pops as many enqueued requests as possible, and returns
    /// the serialized data. If there are no requests enqueued,
    /// this returns `None`.
    pub fn pop_queue(&mut self) -> Option<Vec<u8>> {
        // If we need to acknowledge messages, this notification goes
        // in with the rest of requests so that we can also include it.
        if !self.pending_ack.is_empty() {
            let msg_ids = std::mem::take(&mut self.pending_ack);
            self.enqueue_body(
                tl::enums::MsgsAck::MsgsAck(tl::types::MsgsAck { msg_ids }).to_bytes(),
                false,
            );
        }

        // If there is nothing in the queue, we don't have to do any work.
        if self.message_queue.is_empty() {
            return None;
        }

        // Try to pop as many messages as we possibly can fit in a container.
        // This will reduce overhead from encryption and outer network calls.
        let mut batch_size = 0;

        // Count how many messages we can send in a single batch
        // and determine the size needed to serialize all of them.
        //
        // We can batch `MAXIMUM_LENGTH` requests at most,
        // and their size cannot exceed `MAXIMUM_SIZE`.
        let batch_len = self
            .message_queue
            .iter()
            .take(manual_tl::MessageContainer::MAXIMUM_LENGTH)
            .take_while(|message| {
                if batch_size + message.size() < manual_tl::MessageContainer::MAXIMUM_SIZE {
                    batch_size += message.size();
                    true
                } else {
                    false
                }
            })
            .count();

        // If we're sending more than one, add room for the
        // `MessageContainer` header and its own message too.
        if batch_len > 1 {
            batch_size +=
                manual_tl::Message::SIZE_OVERHEAD + manual_tl::MessageContainer::SIZE_OVERHEAD;
        }

        // Allocate enough size and pop that many requests
        let mut buf = io::Cursor::new(Vec::with_capacity(batch_size));

        // If we're sending more than one, write the `MessageContainer` header.
        // This should be the moral equivalent of `MessageContainer.serialize(...)`.
        if batch_len > 1 {
            // This should be the moral equivalent of `enqueue_body`
            // and `Message::serialize`.
            let msg_id = self.get_new_msg_id();
            let seq_no = self.get_seq_no(false);

            // Safe to unwrap because we're serializing into a memory buffer.
            msg_id.serialize(&mut buf).unwrap();
            seq_no.serialize(&mut buf).unwrap();
            ((batch_size - manual_tl::Message::SIZE_OVERHEAD) as i32)
                .serialize(&mut buf)
                .unwrap();

            manual_tl::MessageContainer::CONSTRUCTOR_ID
                .serialize(&mut buf)
                .unwrap();
            (batch_len as i32).serialize(&mut buf).unwrap();
        }

        // Finally, pop that many requests and write them to the buffer.
        (0..batch_len).for_each(|_| {
            // Safe to unwrap because the length cannot exceed the queue's.
            let message = self.message_queue.pop_front().unwrap();
            // Safe to unwrap because we're serializing into a memory buffer.
            message.serialize(&mut buf).unwrap();
        });

        // The buffer is full, encrypt it and return the data ready to be
        // sent over the network!
        Some(buf.into_inner())
    }

    /// Generates a new unique message ID based on the current
    /// time (in ms) since epoch, applying a known time offset.
    fn get_new_msg_id(&mut self) -> i64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time is before epoch");

        let nanoseconds = now.subsec_nanos() as u64;
        let mut new_msg_id = ((now.as_secs() << 32) | (nanoseconds << 2)) as i64;

        if self.last_msg_id >= new_msg_id {
            new_msg_id = self.last_msg_id + 4;
        }

        self.last_msg_id = new_msg_id;
        new_msg_id
    }

    /// Generates the next sequence number depending on whether
    /// it should be for a content-related query or not.
    fn get_seq_no(&mut self, content_related: bool) -> i32 {
        if content_related {
            let result = self.sequence * 2 + 1;
            self.sequence += 1;
            result
        } else {
            self.sequence * 2
        }
    }

    /// Encrypts the data returned by `pop_queue` to be able to send it
    /// over the network, using the current authorization key as indicated
    /// by the [MTProto 2.0 guidelines].
    ///
    /// [MTProto 2.0 guidelines]: https://core.telegram.org/mtproto/description.
    pub fn encrypt_message_data(&self, plaintext: Vec<u8>) -> Vec<u8> {
        // TODO maybe this should be part of `pop_queue` which has the entire buffer.
        //      IIRC plaintext mtproto also needs this but with salt 0
        let mut buffer = io::Cursor::new(Vec::with_capacity(8 + 8 + plaintext.len()));

        // Prepend salt (8 bytes) and client_id (8 bytes) to the plaintext
        // Safe to unwrap because we have an in-memory buffer
        self.salt.serialize(&mut buffer).unwrap();
        self.client_id.serialize(&mut buffer).unwrap();
        buffer.write_all(&plaintext).unwrap();
        let plaintext = buffer.into_inner();

        encrypt_data_v2(&plaintext, &self.auth_key, Side::Client)
    }

    /// Decrypts a response packet and handles its contents. If
    /// the response belonged to a previous request, it is returned.
    pub fn decrypt_response(&mut self) -> Option<Response> {
        unimplemented!("recv handler not implemented");
    }

    fn process_message(&mut self, message: manual_tl::Message) {
        self.pending_ack.push(message.msg_id);
        let constructor_id = match message.constructor_id() {
            Ok(x) => x,
            Err(e) => {
                // TODO propagate
                eprintln!("failed to peek constructor ID from message: {:?}", e);
                return;
            }
        };

        match constructor_id {
            manual_tl::RpcResult::CONSTRUCTOR_ID => self.handle_rpc_result(),
            manual_tl::MessageContainer::CONSTRUCTOR_ID => self.handle_container(),
            tl::types::Pong::CONSTRUCTOR_ID => self.handle_pong(),
            tl::types::BadServerSalt::CONSTRUCTOR_ID => self.handle_bad_server_salt(),
            tl::types::BadMsgNotification::CONSTRUCTOR_ID => self.handle_bad_notification(),
            tl::types::MsgDetailedInfo::CONSTRUCTOR_ID => self.handle_detailed_info(),
            tl::types::MsgNewDetailedInfo::CONSTRUCTOR_ID => self.handle_new_detailed_info(),
            tl::types::NewSessionCreated::CONSTRUCTOR_ID => self.handle_new_session_created(),
            tl::types::MsgsAck::CONSTRUCTOR_ID => self.handle_ack(),
            tl::types::FutureSalts::CONSTRUCTOR_ID => self.handle_future_salts(),
            tl::types::MsgsStateReq::CONSTRUCTOR_ID => self.handle_state_forgotten(),
            tl::types::MsgResendReq::CONSTRUCTOR_ID => self.handle_state_forgotten(),
            tl::types::MsgsAllInfo::CONSTRUCTOR_ID => self.handle_msg_all(),
            _ => self.handle_update(),
        }
        unimplemented!();
    }

    fn handle_rpc_result(&self) {
        unimplemented!();
    }
    fn handle_container(&self) {
        unimplemented!();
    }
    fn handle_gzip_packed(&self) {
        unimplemented!();
    }
    fn handle_pong(&self) {
        unimplemented!();
    }
    fn handle_bad_server_salt(&self) {
        unimplemented!();
    }
    fn handle_bad_notification(&self) {
        unimplemented!();
    }
    fn handle_detailed_info(&self) {
        unimplemented!();
    }
    fn handle_new_detailed_info(&self) {
        unimplemented!();
    }
    fn handle_new_session_created(&self) {
        unimplemented!();
    }
    fn handle_ack(&self) {
        unimplemented!();
    }
    fn handle_future_salts(&self) {
        unimplemented!();
    }
    fn handle_state_forgotten(&self) {
        unimplemented!();
    }
    fn handle_msg_all(&self) {
        unimplemented!();
    }
    fn handle_update(&self) {
        unimplemented!();
    }
}