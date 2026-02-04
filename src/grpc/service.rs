use sentiric_contracts::sentiric::sip::v1::{
    b2bua_service_server::B2buaService,
    InitiateCallRequest, InitiateCallResponse,
    TransferCallRequest, TransferCallResponse,
};
use tonic::{Request, Response, Status};
use tracing::{info, instrument, error, warn};
use std::sync::Arc;
use crate::sip::engine::B2BuaEngine;
use uuid::Uuid;

pub struct MyB2BuaService {
    engine: Arc<B2BuaEngine>,
}

impl MyB2BuaService {
    pub fn new(engine: Arc<B2BuaEngine>) -> Self {
        Self { engine }
    }
}

#[tonic::async_trait]
impl B2buaService for MyB2BuaService {
    
    #[instrument(skip(self), fields(from = %request.get_ref().from_uri, to = %request.get_ref().to_uri))]
    async fn initiate_call(
        &self,
        request: Request<InitiateCallRequest>,
    ) -> Result<Response<InitiateCallResponse>, Status> {
        let req = request.into_inner();
        
        let call_id = if req.call_id.is_empty() {
             Uuid::new_v4().to_string()
        } else {
             req.call_id
        };

        info!("🚀 [RPC] Dış arama başlatma emri alındı: {}", call_id);

        // Engine içindeki yeni metodu çağır
        match self.engine.send_outbound_invite(&call_id, &req.from_uri, &req.to_uri).await {
            Ok(_) => {
                Ok(Response::new(InitiateCallResponse {
                    success: true,
                    new_call_id: call_id,
                }))
            },
            Err(e) => {
                error!("❌ [RPC] Arama başlatılamadı: {}", e);
                Err(Status::internal(e.to_string()))
            }
        }
    }

    async fn transfer_call(&self, _req: Request<TransferCallRequest>) -> Result<Response<TransferCallResponse>, Status> {
        warn!("⚠️ TransferCall henüz desteklenmiyor.");
        Err(Status::unimplemented("Transfer not implemented"))
    }
}