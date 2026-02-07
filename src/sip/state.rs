// sentiric-b2bua-service/src/sip/state.rs

use std::sync::Arc;
use dashmap::DashMap;
use sentiric_sip_core::transaction::SipTransaction;
use sentiric_rtp_core::RtpEndpoint;

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
    pub from_uri: String,
    pub to_uri: String,
    pub rtp_port: u32,
    pub local_tag: String,
    pub endpoint: RtpEndpoint,
    pub active_transaction: Option<SipTransaction>,
}

pub type CallStore = Arc<DashMap<String, CallSession>>;

pub fn new_store() -> CallStore {
    Arc::new(DashMap::new())
}