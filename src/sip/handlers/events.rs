// sentiric-b2bua-service/src/sip/handlers/events.rs
use std::sync::Arc;
use prost_types::Timestamp;
use prost::Message;
use std::time::SystemTime;
use sentiric_contracts::sentiric::event::v1::{CallStartedEvent, CallEndedEvent, MediaInfo};
use sentiric_contracts::sentiric::dialplan::v1::ResolveDialplanResponse;
use crate::rabbitmq::RabbitMqClient;

pub struct EventManager {
    rabbitmq: Arc<RabbitMqClient>,
}

impl EventManager {
    pub fn new(rabbitmq: Arc<RabbitMqClient>) -> Self {
        Self { rabbitmq }
    }

    pub async fn publish_call_started(
        &self, call_id: &str, server_port: u32, caller_rtp: &str, from: &str, to: &str,
        dialplan_res: Option<ResolveDialplanResponse>
    ) {
        let event = CallStartedEvent { 
            event_type: "call.started".to_string(), 
            // [HATA 3 ÇÖZÜMÜ]: Uuid::new_v4() yerine orjinal call_id'yi trace_id yapıyoruz.
            trace_id: call_id.to_string(), 
            call_id: call_id.to_string(), 
            from_uri: from.to_string(), 
            to_uri: to.to_string(), 
            timestamp: Some(Timestamp::from(SystemTime::now())), 
            dialplan_resolution: dialplan_res, 
            media_info: Some(MediaInfo { 
                caller_rtp_addr: caller_rtp.to_string(), 
                server_rtp_port: server_port 
            }) 
        };
        let _ = self.rabbitmq.publish_event_bytes("call.started", &event.encode_to_vec()).await;
    }

    pub async fn publish_call_ended(&self, call_id: &str) {
        let event = CallEndedEvent { 
            event_type: "call.ended".to_string(), 
            // [HATA 3 ÇÖZÜMÜ]: Uuid::new_v4() yerine orjinal call_id'yi trace_id yapıyoruz.
            trace_id: call_id.to_string(), 
            call_id: call_id.to_string(), 
            timestamp: Some(Timestamp::from(SystemTime::now())), 
            reason: "normal_clearing".to_string() 
        };
        let _ = self.rabbitmq.publish_event_bytes("call.ended", &event.encode_to_vec()).await;
    }
}