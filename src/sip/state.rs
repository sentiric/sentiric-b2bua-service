use std::sync::Arc;
use std::net::SocketAddr;
use dashmap::DashMap;
use sentiric_sip_core::SipPacket;

#[derive(Debug, Clone, PartialEq)]
pub enum CallState {
    Null,
    Trying,
    Ringing,
    Established,
    Terminated,
}

#[derive(Debug, Clone)]
pub struct CallSession {
    pub call_id: String,
    pub state: CallState,
    
    // Kimlikler
    pub from_uri: String,
    pub to_uri: String,
    
    // Medya Bilgileri (Media Service'ten alınan)
    pub rtp_port: u32,
    
    // SIP Transaction State
    pub local_tag: String,
    pub remote_tag: Option<String>,
    
    // Routing Bilgisi
    pub caller_addr: Option<SocketAddr>, // Bacak A (Arayan)
    pub callee_addr: Option<SocketAddr>, // Bacak B (Aranan)
    
    // Bridging
    pub is_bridged: bool,   // Eğer true ise, medya P2P veya Bridge modundadır
    pub peer_call_id: Option<String>, // Diğer bacağın ID'si (Bridge durumunda)

    // Idempotency & Retransmission Cache
    pub last_invite_request: Option<SipPacket>,
    pub last_response: Option<SipPacket>,
}

// Global Store Tipi
pub type CallStore = Arc<DashMap<String, CallSession>>;

pub fn new_store() -> CallStore {
    Arc::new(DashMap::new())
}