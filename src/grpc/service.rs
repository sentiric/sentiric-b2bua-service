// sentiric-b2bua-service/src/grpc/service.rs
use sentiric_contracts::sentiric::sip::v1::{
    b2bua_service_server::B2buaService,
    InitiateCallRequest, InitiateCallResponse,
    TransferCallRequest, TransferCallResponse,
};
use tonic::{Request, Response, Status};
use tracing::{info, instrument};

pub struct MyB2BuaService {}

#[tonic::async_trait]
impl B2buaService for MyB2BuaService {
    
    #[instrument(skip_all, fields(from = %request.get_ref().from_uri, to = %request.get_ref().to_uri))]
    async fn initiate_call(
        &self,
        request: Request<InitiateCallRequest>,
    ) -> Result<Response<InitiateCallResponse>, Status> {
        info!("InitiateCall RPC isteği alındı. Giden çağrı başlatılıyor...");
        let _req = request.into_inner(); 
        
        let new_call_id = format!("call-{}", rand::random::<u32>());

        Ok(Response::new(InitiateCallResponse {
            success: true,
            new_call_id: new_call_id,
        }))
    }

    #[instrument(skip_all, fields(existing_call = %request.get_ref().existing_call_id))]
    async fn transfer_call(
        &self,
        request: Request<TransferCallRequest>,
    ) -> Result<Response<TransferCallResponse>, Status> {
        info!("TransferCall RPC isteği alındı. Mevcut çağrı aktarılıyor...");
        let _req = request.into_inner(); 

        Ok(Response::new(TransferCallResponse {
            success: true,
        }))
    }
}