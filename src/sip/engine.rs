// sentiric-b2bua-service/src/sip/engine.rs

use std::sync::Arc;
use tokio::sync::Mutex;
use std::net::SocketAddr;
use tracing::{info, debug};
use sentiric_sip_core::{
    SipPacket, Method, HeaderName, Header, 
    utils as sip_utils, 
    TransactionEngine, TransactionAction
};
use crate::grpc::client::InternalClients;
use crate::config::AppConfig;
use crate::sip::state::{CallStore, CallSession, CallState};
use crate::rabbitmq::RabbitMqClient;
use crate::sip::handlers::media::MediaManager;
use crate::sip::handlers::events::EventManager;
use crate::sip::handlers::calls::CallHandler; // Uzman handler eklendi

pub struct B2BuaEngine {
    config: Arc<AppConfig>,
    calls: CallStore,
    transport: Arc<sentiric_sip_core::SipTransport>,
    call_handler: CallHandler, // Sorumluluk devredildi
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
        let call_handler = CallHandler::new(
            config.clone(), 
            clients.clone(), 
            calls.clone(), 
            media_mgr, 
            event_mgr
        );

        Self { 
            config, 
            calls, 
            transport,
            call_handler,
        }
    }

    /// [RPC] Dış Arama Başlatma
    pub async fn send_outbound_invite(&self, call_id: &str, from_uri: &str, to_uri: &str) -> anyhow::Result<()> {
        // Not: Outbound logic CallHandler'a da taşınabilir ancak gRPC endpoint'i 
        // doğrudan engine'e bağlı olduğu için burada kalması veya handler'a paslanması opsiyoneldir.
        // Şimdilik temiz bir delegation yapıyoruz.
        info!("Initiating outbound call via engine: {}", call_id);
        
        // Bu metodun içeriği CallHandler içindeki yeni bir metod olabilir.
        // Ancak Anti-Lazy kuralı gereği engine'in orkestrasyon rolünü burada bitirelim.
        
        let mut invite = SipPacket::new_request(Method::Invite, to_uri.to_string());
        let local_tag = sip_utils::generate_tag("sentiric-out");
        
        invite.headers.push(Header::new(HeaderName::From, format!("<{}>;tag={}", from_uri, local_tag)));
        invite.headers.push(Header::new(HeaderName::To, format!("<{}>", to_uri)));
        invite.headers.push(Header::new(HeaderName::CallId, call_id.to_string()));
        invite.headers.push(Header::new(HeaderName::CSeq, "1 INVITE".to_string()));
        
        let session = CallSession {
           call_id: call_id.to_string(),
           state: CallState::Trying,
           from_uri: from_uri.to_string(),
           to_uri: to_uri.to_string(),
           rtp_port: 0, // Outbound SDP negotiation sonrası atanacak
           local_tag,
           remote_tag: None,
           caller_addr: None,
           callee_addr: None,
           is_bridged: false,
           active_transaction: None, 
       };
       self.calls.insert(call_id.to_string(), session);

       let proxy_addr: SocketAddr = self.config.proxy_sip_addr.parse()?;
       self.transport.send(&invite.to_bytes(), proxy_addr).await?;
       Ok(())
    }

    pub async fn handle_packet(&self, packet: SipPacket, src_addr: SocketAddr) {
        let call_id = packet.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
        
        // 1. Transaction Kontrolü (Ağ Seviyesi)
        let action = if let Some(session) = self.calls.get(&call_id) {
             TransactionEngine::check(&session.active_transaction, &packet)
        } else {
             TransactionAction::ForwardToApp
        };

        match action {
            TransactionAction::RetransmitResponse(cached_resp) => {
                let _ = self.transport.send(&cached_resp.to_bytes(), src_addr).await;
                return;
            },
            TransactionAction::Ignore => return,
            TransactionAction::ForwardToApp => {}
        }

        // 2. Uzman Handler'a Yönlendirme (Mantıksal Katman)
        if packet.is_request {
            match packet.method {
                Method::Invite => self.call_handler.process_invite(self.transport.clone(), packet, src_addr).await,
                Method::Ack => self.call_handler.process_ack(&call_id).await,
                Method::Bye => self.call_handler.process_bye(self.transport.clone(), packet, src_addr).await,
                _ => debug!("Ignored request: {:?}", packet.method),
            }
        } else {
            // Yanıtları işleme mantığı (180 Ringing, 200 OK vb.)
            self.handle_response(packet, src_addr).await;
        }
    }

    async fn handle_response(&self, res: SipPacket, _src_addr: SocketAddr) {
        // Outbound yanıtları veya diğer statü güncellemeleri
        let call_id = res.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
        debug!("Response received for {}: {}", call_id, res.status_code);
    }
}