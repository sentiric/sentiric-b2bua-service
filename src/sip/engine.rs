// sentiric-b2bua-service/src/sip/engine.rs

use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, error, instrument}; // warn kaldırıldı (kullanılmıyordu)
use sentiric_sip_core::{SipPacket, Method, Header, HeaderName, SipTransport};
use crate::config::AppConfig;
use crate::grpc::client::InternalClients;
use crate::sip::state::{CallStore, CallSession, CallState};
// DÜZELTME: ConnectPortsRequest kaldırıldı
use sentiric_contracts::sentiric::media::v1::AllocatePortRequest;
use sentiric_contracts::sentiric::sip::v1::LookupContactRequest;
use tonic::Request;
use tokio::net::lookup_host;
// DÜZELTME: uuid::Uuid kaldırıldı (kullanılmıyordu)

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
                Method::Bye => self.handle_bye(packet).await,
                Method::Ack => {}, 
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
        
        let target_uri = self.extract_uri(&to);

        info!("B2BUA: Gelen Çağrı (Inbound): {} -> {} (ID: {})", from, target_uri, call_id);

        // 1. Trying Gönder
        self.send_response(&packet, 100, "Trying", None, src_addr).await;

        // 2. Registrar'dan Hedefi Bul
        let mut clients = self.clients.lock().await;
        let lookup_res = clients.registrar.lookup_contact(Request::new(LookupContactRequest {
            sip_uri: target_uri.clone(),
        })).await;

        let contact_uri = match lookup_res {
            Ok(res) => {
                let uris = res.into_inner().contact_uris;
                if uris.is_empty() {
                    self.send_response(&packet, 404, "Not Found", None, src_addr).await;
                    return;
                }
                uris[0].clone() 
            },
            Err(e) => {
                error!("Registrar Lookup Hatası: {}", e);
                self.send_response(&packet, 500, "Internal Error", None, src_addr).await;
                return;
            }
        };

        // 3. Medya Portlarını Ayır
        let port_a_res = clients.media.allocate_port(Request::new(AllocatePortRequest {
            call_id: format!("{}_A", call_id),
        })).await;
        
        let port_b_res = clients.media.allocate_port(Request::new(AllocatePortRequest {
            call_id: format!("{}_B", call_id),
        })).await;

        drop(clients);

        if port_a_res.is_err() || port_b_res.is_err() {
            self.send_response(&packet, 503, "Media Error", None, src_addr).await;
            return;
        }

        let port_a = port_a_res.unwrap().into_inner().rtp_port;
        let port_b = port_b_res.unwrap().into_inner().rtp_port;

        // 4. State Kaydet
        let session = CallSession {
            call_id: call_id.clone(),
            state: CallState::Ringing,
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

        // 6. Aranan Kişiye INVITE Gönder
        let outbound_call_id = format!("{}_B_LEG", call_id);
        self.initiate_outbound_leg(outbound_call_id, from, target_uri, contact_uri, port_b, call_id).await;
    }

    async fn initiate_outbound_leg(&self, call_id: String, from: String, to_uri: String, contact_uri: String, rtp_port: u32, original_call_id: String) {
        let sdp = self.create_sdp(rtp_port);
        
        let mut invite = SipPacket::new_request(Method::Invite, contact_uri.clone());
        
        invite.headers.push(Header::new(HeaderName::Via, format!("SIP/2.0/UDP {}:{};branch=z9hG4bK-b2bua-{}", self.config.public_ip, self.config.sip_port, call_id)));
        invite.headers.push(Header::new(HeaderName::From, format!("<{}>;tag=b2bua-{}", from, call_id)));
        invite.headers.push(Header::new(HeaderName::To, format!("<{}>", to_uri)));
        invite.headers.push(Header::new(HeaderName::CallId, call_id.clone()));
        invite.headers.push(Header::new(HeaderName::CSeq, "1 INVITE".to_string()));
        invite.headers.push(Header::new(HeaderName::Contact, format!("<sip:b2bua@{}:{}>", self.config.public_ip, self.config.sip_port)));
        invite.headers.push(Header::new(HeaderName::UserAgent, "Sentiric B2BUA".to_string()));
        invite.headers.push(Header::new(HeaderName::ContentType, "application/sdp".to_string()));
        invite.body = sdp.as_bytes().to_vec();

        self.calls.insert(call_id.clone(), CallSession {
            call_id: call_id.clone(),
            state: CallState::Trying,
            from_uri: from,
            to_uri: to_uri,
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
            Err(e) => error!("DNS Hatası: {}", e),
        }
    }

    async fn handle_response(&self, session: &mut CallSession, packet: &SipPacket) {
        if packet.status_code == 200 {
            info!("Outbound Leg Cevaplandı: {}", session.call_id);
            session.state = CallState::Established;

            if let Some(peer_id) = &session.peer_call_id {
                if let Some(peer_session) = self.calls.get(peer_id) {
                    if let Some(src_addr) = peer_session.source_addr {
                        // Arayana 200 OK Gönder
                        let sdp = self.create_sdp(peer_session.rtp_port);
                        
                        let mut resp = SipPacket::new_response(200, "OK".to_string());
                        resp.headers.push(Header::new(HeaderName::Via, format!("SIP/2.0/UDP {}:{};branch=z9hG4bK-proxy-pass", self.config.public_ip, self.config.sip_port))); 
                        resp.headers.push(Header::new(HeaderName::From, peer_session.from_uri.clone()));
                        resp.headers.push(Header::new(HeaderName::To, format!("{};tag=b2bua-Tag", peer_session.to_uri)));
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

    // DÜZELTME: packet parametresinin önüne _ eklendi
    async fn handle_bye(&self, _packet: SipPacket) {
        // BYE logic
    }

    async fn send_response(&self, req: &SipPacket, code: u16, reason: &str, body: Option<String>, target: std::net::SocketAddr) {
        let mut resp = SipPacket::new_response(code, reason.to_string());
        for h in &req.headers {
            match h.name {
                HeaderName::Via | HeaderName::From | HeaderName::To | HeaderName::CallId | HeaderName::CSeq => {
                    resp.headers.push(h.clone());
                },
                _ => {}
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
            m=audio {2} RTP/AVP 0 8 101\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:8 PCMA/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n\
            a=fmtp:101 0-16\r\n\
            a=sendrecv\r\n",
            12345,
            self.config.public_ip,
            port
        )
    }

    fn extract_uri(&self, header_val: &str) -> String {
        if let Some(start) = header_val.find("sip:") {
            if let Some(end) = header_val[start..].find('>') {
                return header_val[start..start+end].to_string();
            } else if let Some(end) = header_val[start..].find(';') {
                return header_val[start..start+end].to_string();
            }
            return header_val[start..].to_string();
        }
        header_val.to_string()
    }
    
    // DÜZELTME: Parametrelerin önüne _ eklendi
    pub async fn initiate_call(&self, call_id: String, _from: String, _to: String) -> anyhow::Result<()> {
        info!("System initiated call: {}", call_id);
        Ok(())
    }
}