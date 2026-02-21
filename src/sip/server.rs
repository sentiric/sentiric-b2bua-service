use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info, warn, debug};
use sentiric_sip_core::{SipTransport, parser, HeaderName}; // HeaderName eklendi
use crate::sip::engine::B2BuaEngine;

pub struct SipServer {
    engine: Arc<B2BuaEngine>,
    transport: Arc<SipTransport>,
}

impl SipServer {
    pub fn new(engine: Arc<B2BuaEngine>, transport: Arc<SipTransport>) -> Self {
        Self { engine, transport }
    }

    pub async fn run(self, mut shutdown_rx: mpsc::Receiver<()>) {
        info!(event="SIP_SERVER_ACTIVE", "📡 B2BUA SIP Listener aktif.");

        let mut buf = vec![0u8; 65535];
        let socket = self.transport.get_socket();

        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    info!(event="SIP_SHUTDOWN", "SIP Server kapatılıyor...");
                    break;
                }
                
                res = socket.recv_from(&mut buf) => {
                    match res {
                        Ok((len, src_addr)) => {
                            if len < 4 || buf[..len].iter().all(|&b| b == b'\r' || b == b'\n' || b == 0) {
                                continue;
                            }

                            let data = &buf[..len];
                            match parser::parse(data) {
                                Ok(packet) => {
                                    // INGRESS LOG
                                    let call_id = packet.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
                                    let method = packet.method.as_str();
                                    
                                    debug!(
                                        event = "SIP_PACKET_RECEIVED",
                                        trace_id = %call_id,
                                        sip.call_id = %call_id,
                                        sip.method = %method,
                                        net.src.ip = %src_addr.ip(),
                                        net.src.port = src_addr.port(),
                                        "📥 SIP paketi alındı"
                                    );

                                    let engine = self.engine.clone();
                                    tokio::spawn(async move {
                                        engine.handle_packet(packet, src_addr).await;
                                    });
                                },
                                Err(e) => {
                                    warn!(event="SIP_PARSE_ERROR", error=%e, "SIP parse hatası");
                                }
                            }
                        },
                        Err(e) => error!(event="UDP_ERROR", error=%e, "UDP Error"),
                    }
                }
            }
        }
    }
}