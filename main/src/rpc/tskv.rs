use std::pin::Pin;
use std::sync::Arc;

use coordinator::errors::{CoordinatorError, CoordinatorResult};
use coordinator::service::CoordinatorRef;
use coordinator::{FAILED_RESPONSE_CODE, SUCCESS_RESPONSE_CODE};
use futures::{Stream, TryStreamExt};
use meta::model::MetaRef;
use metrics::metric_register::MetricsRegister;
use models::meta_data::VnodeInfo;
use models::predicate::domain::{self, QueryArgs, QueryExpr};
use models::record_batch_encode;
use models::schema::TableColumn;
use protos::kv_service::tskv_service_server::TskvService;
use protos::kv_service::*;
use protos::models::{PingBody, PingBodyBuilder};
use tokio::io::AsyncReadExt;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Extensions, Request, Response, Status};
use trace::{debug, error, info, SpanContext, SpanExt, SpanRecorder};
use tskv::error::Result as TskvResult;
use tskv::reader::query_executor::QueryExecutor;
use tskv::reader::serialize::TonicRecordBatchEncoder;
use tskv::reader::{QueryOption, SendableTskvRecordBatchStream};
use tskv::EngineRef;

type ResponseStream<T> = Pin<Box<dyn Stream<Item = Result<T, tonic::Status>> + Send>>;

#[derive(Clone)]
pub struct TskvServiceImpl {
    pub runtime: Arc<Runtime>,
    pub kv_inst: EngineRef,
    pub coord: CoordinatorRef,
    pub metrics_register: Arc<MetricsRegister>,
    pub grpc_enable_gzip: bool,
}

impl TskvServiceImpl {
    fn status_response(
        &self,
        code: i32,
        data: String,
    ) -> Result<tonic::Response<StatusResponse>, tonic::Status> {
        Ok(tonic::Response::new(StatusResponse { code, data }))
    }

    fn bytes_response(
        &self,
        code: i32,
        data: Vec<u8>,
    ) -> Result<tonic::Response<BatchBytesResponse>, tonic::Status> {
        Ok(tonic::Response::new(BatchBytesResponse { code, data }))
    }

    fn tonic_status(&self, msg: String) -> tonic::Status {
        tonic::Status::new(tonic::Code::Internal, msg)
    }

    async fn warp_exec_admin_command(
        &self,
        tenant: &str,
        command: &admin_command_request::Command,
    ) -> CoordinatorResult<()> {
        let result = match command {
            admin_command_request::Command::CompactVnode(command) => {
                info!("compact vnodes: {:?}", command.vnode_ids);

                self.kv_inst
                    .compact(command.vnode_ids.clone())
                    .await
                    .map_err(|err| err.into())
            }

            admin_command_request::Command::AddRaftFollower(command) => {
                let raft_manager = self.coord.raft_manager();
                raft_manager
                    .add_follower_to_group(
                        tenant,
                        &command.db_name,
                        command.follower_nid,
                        command.replica_id,
                    )
                    .await
            }

            admin_command_request::Command::RemoveRaftNode(command) => {
                let raft_manager = self.coord.raft_manager();
                raft_manager
                    .remove_node_from_group(
                        tenant,
                        &command.db_name,
                        command.vnode_id,
                        command.replica_id,
                    )
                    .await
            }

            admin_command_request::Command::DestoryRaftGroup(command) => {
                let raft_manager = self.coord.raft_manager();
                raft_manager
                    .destory_replica_group(tenant, &command.db_name, command.replica_id)
                    .await
            }
        };

        info!("admin command: {:?}, result: {:?}", command, result);
        if let Err(CoordinatorError::RaftForwardToLeader {
            replica_id: _,
            leader_vnode_id: vnode_id,
        }) = result
        {
            let meta = self.coord.meta_manager();
            let info = coordinator::get_vnode_all_info(meta, tenant, vnode_id).await?;
            let req = AdminCommandRequest {
                tenant: tenant.to_string(),
                command: Some(command.clone()),
            };

            return self
                .coord
                .exec_admin_command_on_node(info.node_id, req)
                .await;
        }

        result
    }

    async fn admin_fetch_vnode_checksum(
        &self,
        _tenant: &str,
        request: &FetchVnodeChecksumRequest,
    ) -> Result<tonic::Response<BatchBytesResponse>, tonic::Status> {
        match self.kv_inst.get_vnode_hash_tree(request.vnode_id).await {
            Ok(record) => match record_batch_encode(&record) {
                Ok(bytes) => self.bytes_response(SUCCESS_RESPONSE_CODE, bytes),
                Err(_) => self.bytes_response(FAILED_RESPONSE_CODE, vec![]),
            },
            // TODO(zipper): Add error message in BatchBytesResponse
            Err(_) => self.bytes_response(FAILED_RESPONSE_CODE, vec![]),
        }
    }

    fn query_record_batch_exec(
        self,
        args: QueryArgs,
        expr: QueryExpr,
        aggs: Option<Vec<TableColumn>>,
        span_ctx: Option<&SpanContext>,
    ) -> TskvResult<SendableTskvRecordBatchStream> {
        let option = QueryOption::new(
            args.batch_size,
            expr.split,
            aggs,
            Arc::new(expr.df_schema),
            expr.table_schema,
        );

        let meta = self.coord.meta_manager();
        let node_id = meta.node_id();
        let mut vnodes = Vec::with_capacity(args.vnode_ids.len());
        for id in args.vnode_ids.iter() {
            vnodes.push(VnodeInfo::new(*id, node_id))
        }

        let executor = QueryExecutor::new(option, self.runtime.clone(), meta, self.kv_inst.clone());
        executor.local_node_executor(vnodes, span_ctx)
    }

    fn tag_scan_exec(
        args: QueryArgs,
        expr: QueryExpr,
        meta: MetaRef,
        run_time: Arc<Runtime>,
        kv_inst: EngineRef,
        span_ctx: Option<&SpanContext>,
    ) -> TskvResult<SendableTskvRecordBatchStream> {
        let option = QueryOption::new(
            args.batch_size,
            expr.split,
            None,
            Arc::new(expr.df_schema),
            expr.table_schema,
        );

        let node_id = meta.node_id();
        let vnodes = args
            .vnode_ids
            .iter()
            .map(|id| VnodeInfo::new(*id, node_id))
            .collect::<Vec<_>>();

        let executor = QueryExecutor::new(option, run_time, meta, kv_inst);
        executor.local_node_tag_scan(vnodes, span_ctx)
    }
}

#[tonic::async_trait]
impl TskvService for TskvServiceImpl {
    async fn ping(
        &self,
        _request: tonic::Request<PingRequest>,
    ) -> Result<tonic::Response<PingResponse>, tonic::Status> {
        debug!("PING");

        let ping_req = _request.into_inner();
        let ping_body = flatbuffers::root::<PingBody>(&ping_req.body);
        if let Err(e) = ping_body {
            error!("{}", e);
        } else {
            info!("ping_req:body(flatbuffer): {:?}", ping_body);
        }

        let mut fbb = flatbuffers::FlatBufferBuilder::new();
        let payload = fbb.create_vector(b"hello, caller");

        let mut builder = PingBodyBuilder::new(&mut fbb);
        builder.add_payload(payload);
        let root = builder.finish();
        fbb.finish(root, None);

        let finished_data = fbb.finished_data();

        Ok(tonic::Response::new(PingResponse {
            version: 1,
            body: finished_data.to_vec(),
        }))
    }

    async fn exec_raft_write_command(
        &self,
        request: tonic::Request<RaftWriteCommand>,
    ) -> Result<tonic::Response<StatusResponse>, tonic::Status> {
        let span = get_span_recorder(request.extensions(), "grpc write_replica_points");
        let inner = request.into_inner();

        let client = self
            .coord
            .tenant_meta(&inner.tenant)
            .await
            .ok_or(tonic::Status::new(
                tonic::Code::Internal,
                format!("Not Found tenant({}) meta", inner.tenant),
            ))?;

        let replica = client
            .get_replication_set(inner.replica_id)
            .ok_or(tonic::Status::new(
                tonic::Code::Internal,
                format!("Not Found Replica Set({})", inner.replica_id),
            ))?;

        if let Err(err) = self
            .coord
            .exec_write_replica_points(replica, inner, span.span_ctx())
            .await
        {
            self.status_response(FAILED_RESPONSE_CODE, err.to_string())
        } else {
            self.status_response(SUCCESS_RESPONSE_CODE, "".to_string())
        }
    }

    async fn exec_open_raft_node(
        &self,
        request: tonic::Request<OpenRaftNodeRequest>,
    ) -> Result<tonic::Response<StatusResponse>, tonic::Status> {
        let inner = request.into_inner();

        if let Err(err) = self
            .coord
            .raft_manager()
            .exec_open_raft_node(
                &inner.tenant,
                &inner.db_name,
                inner.vnode_id,
                inner.replica_id,
            )
            .await
        {
            self.status_response(FAILED_RESPONSE_CODE, err.to_string())
        } else {
            self.status_response(SUCCESS_RESPONSE_CODE, "".to_string())
        }
    }

    async fn exec_drop_raft_node(
        &self,
        request: tonic::Request<DropRaftNodeRequest>,
    ) -> Result<tonic::Response<StatusResponse>, tonic::Status> {
        let inner = request.into_inner();

        if let Err(err) = self
            .coord
            .raft_manager()
            .exec_drop_raft_node(
                &inner.tenant,
                &inner.db_name,
                inner.vnode_id,
                inner.replica_id,
            )
            .await
        {
            self.status_response(FAILED_RESPONSE_CODE, err.to_string())
        } else {
            self.status_response(SUCCESS_RESPONSE_CODE, "".to_string())
        }
    }

    async fn exec_admin_command(
        &self,
        request: tonic::Request<AdminCommandRequest>,
    ) -> Result<tonic::Response<StatusResponse>, tonic::Status> {
        let inner = request.into_inner();

        if let Some(command) = inner.command {
            if let Err(err) = self.warp_exec_admin_command(&inner.tenant, &command).await {
                self.status_response(FAILED_RESPONSE_CODE, err.to_string())
            } else {
                self.status_response(SUCCESS_RESPONSE_CODE, "".to_string())
            }
        } else {
            self.status_response(FAILED_RESPONSE_CODE, "Command is None".to_string())
        }
    }

    async fn exec_admin_fetch_command(
        &self,
        request: Request<AdminFetchCommandRequest>,
    ) -> Result<Response<BatchBytesResponse>, Status> {
        let inner = request.into_inner();

        if let Some(command) = inner.command {
            match &command {
                admin_fetch_command_request::Command::FetchVnodeChecksum(command) => {
                    self.admin_fetch_vnode_checksum(&inner.tenant, command)
                        .await
                }
            }
        } else {
            self.bytes_response(FAILED_RESPONSE_CODE, vec![])
        }
    }

    type DownloadFileStream = ResponseStream<BatchBytesResponse>;
    async fn download_file(
        &self,
        request: tonic::Request<DownloadFileRequest>,
    ) -> Result<tonic::Response<Self::DownloadFileStream>, tonic::Status> {
        let inner = request.into_inner();
        let opt = self.kv_inst.get_storage_options();
        let filename = opt.path().join(inner.filename);
        info!("request download file name: {:?}", filename);

        let (send, recv) = mpsc::channel(1024);
        tokio::spawn(async move {
            if let Ok(mut file) = tokio::fs::File::open(filename).await {
                let mut buffer = vec![0; 8 * 1024];
                while let Ok(len) = file.read(&mut buffer).await {
                    if len == 0 {
                        break;
                    }

                    let _ = send
                        .send(Ok(BatchBytesResponse {
                            code: SUCCESS_RESPONSE_CODE,
                            data: (buffer[0..len]).to_vec(),
                        }))
                        .await;
                }
            }
        });

        let out_stream = ReceiverStream::new(recv);

        Ok(tonic::Response::new(Box::pin(out_stream)))
    }

    /// Server streaming response type for the QueryRecordBatch method.
    type QueryRecordBatchStream = ResponseStream<BatchBytesResponse>;
    async fn query_record_batch(
        &self,
        request: tonic::Request<QueryRecordBatchRequest>,
    ) -> Result<tonic::Response<Self::QueryRecordBatchStream>, tonic::Status> {
        let span_recorder = get_span_recorder(request.extensions(), "grpc query_record_batch");
        let inner = request.into_inner();

        let args = match QueryArgs::decode(&inner.args) {
            Ok(args) => args,
            Err(err) => return Err(self.tonic_status(err.to_string())),
        };

        let expr = match QueryExpr::decode(&inner.expr) {
            Ok(expr) => expr,
            Err(err) => return Err(self.tonic_status(err.to_string())),
        };

        let aggs = match domain::decode_agg(&inner.aggs) {
            Ok(aggs) => aggs,
            Err(err) => return Err(self.tonic_status(err.to_string())),
        };

        let service = self.clone();

        let encoded_stream = {
            let span_recorder = span_recorder.child("RecordBatch encorder stream");

            let stream = TskvServiceImpl::query_record_batch_exec(
                service,
                args,
                expr,
                aggs,
                span_recorder.span_ctx(),
            )?;
            TonicRecordBatchEncoder::new(stream, span_recorder).map_err(Into::into)
        };

        Ok(tonic::Response::new(Box::pin(encoded_stream)))
    }

    type TagScanStream = ResponseStream<BatchBytesResponse>;
    async fn tag_scan(
        &self,
        request: Request<QueryRecordBatchRequest>,
    ) -> Result<Response<Self::TagScanStream>, Status> {
        let span_recorder = get_span_recorder(request.extensions(), "grpc query_record_batch");
        let inner = request.into_inner();

        let args = match QueryArgs::decode(&inner.args) {
            Ok(args) => args,
            Err(err) => return Err(self.tonic_status(err.to_string())),
        };

        let expr = match QueryExpr::decode(&inner.expr) {
            Ok(expr) => expr,
            Err(err) => return Err(self.tonic_status(err.to_string())),
        };

        let stream = {
            let span_recorder = span_recorder.child("RecordBatch encorder stream");
            let stream = TskvServiceImpl::tag_scan_exec(
                args,
                expr,
                self.coord.meta_manager(),
                self.runtime.clone(),
                self.kv_inst.clone(),
                span_recorder.span_ctx(),
            )?;

            TonicRecordBatchEncoder::new(stream, span_recorder).map_err(Into::into)
        };

        Ok(tonic::Response::new(Box::pin(stream)))
    }
}

fn get_span_recorder(extensions: &Extensions, child_span_name: &'static str) -> SpanRecorder {
    let span_context = extensions.get::<SpanContext>();
    SpanRecorder::new(span_context.child_span(child_span_name))
}
