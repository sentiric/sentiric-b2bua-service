// sentiric-b2bua-service/src/sip/engine.rs

use std::sync::Arc;
use tokio::sync::Mutex;
use std::net::SocketAddr;
use tracing::debug;
use sentiric_sip_core::{
    SipPacket, Method, HeaderName,
    transaction::{TransactionEngine, TransactionAction},
};
use crate::grpc::client::InternalClients;
use crate::config::AppConfig;
use crate::sip::store::CallStore;
use crate::rabbitmq::RabbitMqClient;
use crate::sip::handlers::{media::MediaManager, events::EventManager, calls::CallHandler};

pub struct B2BuaEngine {
    calls: CallStore,
    transport: Arc<sentiric_sip_core::SipTransport>,
    call_handler: CallHandler,
}

impl B2BuaEngine {
    pub fn new(
        config: Arc<AppConfig>,
        clients: Arc<Mutex<InternalClients>>,
        calls: CallStore,
        transport: Arc<sentiric_sip_core::SipTransport>,
        rabbitmq: Arc<RabbitMqClient>,
    ) -> Self {
        let media_mgr = MediaManager::new(clients.clone(), config.clone());
        let event_mgr = EventManager::new(rabbitmq);
        let call_handler = CallHandler::new(config, clients, calls.clone(), media_mgr, event_mgr);

        Self { calls, transport, call_handler }
    }

    pub async fn send_outbound_invite(&self, call_id: &str, from_uri: &str, to_uri: &str) -> anyhow::Result<()> {
        self.call_handler.process_outbound_invite(self.transport.clone(), call_id, from_uri, to_uri).await
    }

    pub async fn handle_packet(&self, packet: SipPacket, src_addr: SocketAddr) {
        let call_id = packet.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
        
        let session_opt = self.calls.get(&call_id).await;

        let action = if let Some(session) = session_opt {
             TransactionEngine::check(&session.active_transaction, &packet)
        } else {
             TransactionAction::ForwardToApp
        };

        match action {
            TransactionAction::Retransmit(cached_resp) => {
                let _ = self.transport.send(&cached_resp.to_bytes(), src_addr).await;
            },
            TransactionAction::Ignore => return,
            TransactionAction::ForwardToApp => {
                // [FIX]: is_request() helper kullanımı
                if packet.is_request() {
                    match packet.method {
                        Method::Invite => self.call_handler.process_invite(self.transport.clone(), packet, src_addr).await,
                        Method::Ack => self.call_handler.process_ack(&call_id).await,
                        Method::Bye => self.call_handler.process_bye(self.transport.clone(), packet, src_addr).await,
                        _ => debug!("Method ignored: {:?}", packet.method),
                    }
                }
            }
        }
    }
}