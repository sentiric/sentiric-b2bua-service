// sentiric-b2bua-service/src/sip/engine.rs

use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, error, instrument};
// DÜZELTME: sip-core'dan utils'i import ediyoruz
use sentiric_sip_core::{SipPacket, Method, Header, HeaderName, SipTransport, utils as sip_core_utils};
use crate::config::AppConfig;
use crate::grpc::client::InternalClients;
use crate::sip::state::{CallStore, CallSession, CallState};
use sentiric_contracts::sentiric::media::v1::AllocatePortRequest;
use sentiric_contracts::sentiric::sip::v1::LookupContactRequest;
use tonic::Request;
use tokio::net::lookup_host;

pub struct B2BuaEngine {
    config: Arc<AppConfig>,
    clients: Arc<Mutex<InternalClients>>,
    calls: CallStore,
    transport: Arc<SipTransport>,
}

impl B2BuaEngine {
    pub fn new(
        config: Arc<AppConfig>, 
        clients: Arc<Mutex<InternalClients>>, 
        calls: CallStore,
        transport: Arc<SipTransport>
    ) -> Self {
        Self { config, clients, calls, transport }
    }

    /// Gelen SIP paketlerini işler.
    pub async fn handle_packet(&self, packet: SipPacket, src_addr: std::net::SocketAddr) {
        if packet.is_request {
            match packet.method {
                Method::Invite => self.handle_incoming_invite(packet, src_addr).await,
                Method::Bye => self.handle_bye(&packet).await,
                Method::Ack => self.handle_ack(&packet).await, 
                _ => {}
            }
        } else {
            // RESPONSE HANDLING
            if let Some(call_id) = packet.get_header_value(HeaderName::CallId) {
                if let Some(mut session) = self.calls.get_mut(call_id) {
                    self.handle_response(&mut session, &packet).await;
                }
            }
        }
    }

    /// Dışarıdan gelen INVITE isteğini karşılar (Inbound Leg).
    #[instrument(skip(self, packet))]
    async fn handle_incoming_invite(&self, packet: SipPacket, src_addr: std::net::SocketAddr) {
        let call_id = packet.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
        let from = packet.get_header_value(HeaderName::From).cloned().unwrap_or_default();
        let to = packet.get_header_value(HeaderName::To).cloned().unwrap_or_default();
        
        let target_aor = sip_core_utils::extract_aor(&to);

        info!("B2BUA: Gelen Çağrı (Inbound): {} -> {} (ID: {})", from, target_aor, call_id);

        // 1. Trying Gönder
        self.send_response(&packet, 100, "Trying", None, src_addr).await;

        // 2. Registrar'dan Hedefi Bul
        let mut clients = self.clients.lock().await;
        let lookup_res = clients.registrar.lookup_contact(Request::new(LookupContactRequest {
            sip_uri: target_aor.clone(),
        })).await;

        let contact_uri = match lookup_res {
            Ok(res) => {
                let uris = res.into_inner().contact_uris;
                if uris.is_empty() {
                    error!("Registrar Lookup Başarısız: {} bulunamadı.", target_aor);
                    self.send_response(&packet, 404, "Not Found", None, src_addr).await;
                    return;
                }
                info!("Registrar Lookup Başarılı: {} -> {}", target_aor, uris[0]);
                uris[0].clone() 
            },
            Err(e) => {
                error!("Registrar Lookup Hatası: {}", e);
                self.send_response(&packet, 500, "Internal Server Error", None, src_addr).await;
                return;
            }
        };

        // 3. Medya Portlarını Ayır
        let port_a_res = clients.media.allocate_port(Request::new(AllocatePortRequest {
            call_id: format!("{}_A", call_id),
        })).await;
        
        drop(clients);

        let port_a = match port_a_res {
            Ok(res) => res.into_inner().rtp_port,
            Err(e) => {
                error!("Media Service AllocatePort hatası: {}", e);
                self.send_response(&packet, 503, "Media Error", None, src_addr).await;
                return;
            }
        };

        // 4. State Kaydet (Inbound Leg A)
        let session = CallSession {
            call_id: call_id.clone(),
            state: CallState::Trying,
            from_uri: from.clone(),
            to_uri: to.clone(),
            rtp_port: port_a,
            remote_tag: None,
            peer_call_id: Some(format!("{}_B_LEG", call_id)),
            source_addr: Some(src_addr),
        };
        self.calls.insert(call_id.clone(), session);

        // 5. Ringing Dön
        self.send_response(&packet, 180, "Ringing", None, src_addr).await;

        // 6. Aranan Kişiye INVITE Gönder (Outbound Leg B)
        let outbound_call_id = format!("{}_B_LEG", call_id);
        self.initiate_outbound_leg(outbound_call_id, from, target_aor, contact_uri, port_a, call_id).await;
    }

    async fn initiate_outbound_leg(&self, call_id: String, from_header: String, to_aor: String, contact_uri: String, rtp_port: u32, original_call_id: String) {
        let sdp = self.create_sdp(rtp_port);
        
        let mut invite = SipPacket::new_request(Method::Invite, format!("sip:{}", contact_uri));
        
        let branch = sip_core_utils::generate_branch_id();
        invite.headers.push(Header::new(HeaderName::Via, format!("SIP/2.0/UDP {}:{};branch={}", self.config.public_ip, self.config.sip_port, branch)));
        invite.headers.push(Header::new(HeaderName::From, from_header.clone()));
        invite.headers.push(Header::new(HeaderName::To, format!("<sip:{}>", to_aor)));
        invite.headers.push(Header::new(HeaderName::CallId, call_id.clone()));
        invite.headers.push(Header::new(HeaderName::CSeq, "1 INVITE".to_string()));
        invite.headers.push(Header::new(HeaderName::Contact, format!("<sip:b2bua@{}:{}>", self.config.public_ip, self.config.sip_port)));
        invite.headers.push(Header::new(HeaderName::UserAgent, "Sentiric B2BUA".to_string()));
        invite.headers.push(Header::new(HeaderName::ContentType, "application/sdp".to_string()));
        invite.body = sdp.as_bytes().to_vec();

        self.calls.insert(call_id.clone(), CallSession {
            call_id: call_id.clone(),
            state: CallState::Trying,
            from_uri: from_header,
            to_uri: to_aor,
            rtp_port,
            remote_tag: None,
            peer_call_id: Some(original_call_id),
            source_addr: None,
        });

        let bytes = invite.to_bytes();
        match lookup_host(&self.config.proxy_sip_addr).await {
            Ok(mut addrs) => {
                if let Some(target_addr) = addrs.next() {
                    let _ = self.transport.send(&bytes, target_addr).await;
                    info!("Outbound INVITE gönderildi (ID: {}) -> {}", call_id, target_addr);
                }
            },
            Err(e) => error!("Proxy DNS Hatası ({}): {}", self.config.proxy_sip_addr, e),
        }
    }

    async fn handle_response(&self, session: &mut CallSession, packet: &SipPacket) {
        if packet.status_code >= 200 && packet.status_code < 300 {
            info!("Outbound Leg Cevaplandı (2xx): {}", session.call_id);
            session.state = CallState::Established;

            if let Some(peer_id) = &session.peer_call_id {
                if let Some(mut peer_session) = self.calls.get_mut(peer_id) {
                    peer_session.state = CallState::Established;
                    if let Some(src_addr) = peer_session.source_addr {
                        let sdp = self.create_sdp(peer_session.rtp_port);
                        
                        let mut resp = SipPacket::new_response(200, "OK".to_string());
                        
                        resp.headers.push(Header::new(HeaderName::Via, packet.get_header_value(HeaderName::Via).cloned().unwrap_or_default()));
                        resp.headers.push(Header::new(HeaderName::From, peer_session.from_uri.clone()));
                        resp.headers.push(Header::new(HeaderName::To, format!("{};tag={}", peer_session.to_uri, sip_core_utils::generate_tag(&peer_session.call_id))));
                        resp.headers.push(Header::new(HeaderName::CallId, peer_session.call_id.clone()));
                        resp.headers.push(Header::new(HeaderName::CSeq, "1 INVITE".to_string()));
                        resp.headers.push(Header::new(HeaderName::Contact, format!("<sip:b2bua@{}:{}>", self.config.public_ip, self.config.sip_port)));
                        resp.headers.push(Header::new(HeaderName::ContentType, "application/sdp".to_string()));
                        resp.body = sdp.as_bytes().to_vec();

                        let _ = self.transport.send(&resp.to_bytes(), src_addr).await;
                        info!("Inbound Leg Cevaplandı (Bridged): {}", peer_id);
                    }
                }
            }
        }
    }

    async fn handle_ack(&self, _packet: &SipPacket) {
        // State'i güncelle
    }

    async fn handle_bye(&self, _packet: &SipPacket) {
        // BYE logic
    }

    async fn send_response(&self, req: &SipPacket, code: u16, reason: &str, body: Option<String>, target: std::net::SocketAddr) {
        let mut resp = SipPacket::new_response(code, reason.to_string());
        for h in &req.headers {
            if let HeaderName::Via | HeaderName::From | HeaderName::To | HeaderName::CallId | HeaderName::CSeq = h.name {
                resp.headers.push(h.clone());
            }
        }
        resp.headers.push(Header::new(HeaderName::Server, "Sentiric B2BUA".to_string()));
        
        if let Some(b) = body {
            resp.headers.push(Header::new(HeaderName::ContentType, "application/sdp".to_string()));
            resp.body = b.as_bytes().to_vec();
        } else {
            resp.headers.push(Header::new(HeaderName::ContentLength, "0".to_string()));
        }

        let _ = self.transport.send(&resp.to_bytes(), target).await;
    }

    fn create_sdp(&self, port: u32) -> String {
        format!(
            "v=0\r\n\
            o=- {0} {0} IN IP4 {1}\r\n\
            s=Sentiric_B2BUA\r\n\
            c=IN IP4 {1}\r\n\
            t=0 0\r\n\
            m=audio {2} RTP/AVP 0 8 18 101\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:8 PCMA/8000\r\n\
            a=rtpmap:18 G729/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n\
            a=fmtp:101 0-16\r\n\
            a=sendrecv\r\n",
            rand::random::<u64>(),
            self.config.public_ip,
            port
        )
    }

    // DÜZELTME: Bu fonksiyon tamamen silindi.
    // fn extract_uri(&self, header_val: &str) -> String { ... }

    pub async fn initiate_call(&self, call_id: String, _from: String, _to: String) -> anyhow::Result<()> {
        info!("System initiated call: {}", call_id);
        Ok(())
    }
}