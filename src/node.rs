use std::{cmp::min, error::Error, net::SocketAddr, sync::Arc};
use tokio::{
    sync::Mutex,
    time::{Duration, Instant},
};
use tonic::{transport::Server, Request, Response, Status};

use crate::{
    config::Config, raft::RaftDetails, raft_proto::{
        raft_server::{Raft, RaftServer},
        Byte, EntryReply, EntryRequest, Null, VoteReply, VoteRequest,
    }, state_machine::RaftStateMachine,
};

/// Details necessary to construct a node for raft consensus.
pub struct RaftNode {
    details: Arc<Mutex<RaftDetails>>,
    state: Arc<Mutex<RaftStateMachine>>,
}

impl RaftNode {
    /// Starts a raft node, consisting of server and client gRPC stubs.
    pub async fn start(
        id: u8,
        local_addr: String,
        mut nodes: Vec<String>,
    ) -> Result<Self, Box<dyn Error>> {
        // Keep addr of all nodes but the current one in directory.
        nodes.retain(|x| *x != local_addr);

        // Create shared state
        let raft_details = Arc::new(Mutex::new(RaftDetails::new(id, nodes)));
        let raft_state = Arc::new(Mutex::new(RaftStateMachine::new()));

        // State that is handed over the the server stub on this node
        let raft = Self {
            details: raft_details.clone(),
            state: raft_state.clone(),
        };

        // Server runs on a background thread and handles calls to the node
        tokio::spawn(async move {
            Server::builder()
                .add_service(RaftServer::new(raft))
                .serve(local_addr.parse().unwrap())
                .await
                .unwrap();
        });

        Ok(Self {
            details: raft_details,
            state: raft_state,
        })
    }

    pub async fn run(&self, config: Config) -> Result<(), Box<dyn Error>> {
        let mut clock = Instant::now();

        loop {
            if clock.elapsed() > Duration::from_secs(config.new_rand_election_timeout()) {
                clock = Instant::now();
                self.details.lock().await.start_election().await?;
            }
        }
    }
}

#[tonic::async_trait]
impl Raft for RaftNode {
    async fn request_vote(
        &self,
        request: Request<VoteRequest>,
    ) -> Result<Response<VoteReply>, Status> {
        let request = request.into_inner();
        let details = self.details.lock().await;
        if request.term < details.current_term {
            return Ok(Response::new(VoteReply {
                term: details.current_term,
                grant: false,
            }));
        } else if details.voted_for == details.id {
            return Ok(Response::new(VoteReply {
                term: details.current_term,
                grant: true,
            }));
        }

        Ok(Response::new(VoteReply {
            term: details.current_term,
            grant: false,
        }))
    }

    async fn append_entries(
        &self,
        request: Request<EntryRequest>,
    ) -> Result<Response<EntryReply>, Status> {
        let request = request.into_inner();
        let mut details = self.details.lock().await;
        if request.term < details.current_term {
            return Ok(Response::new(EntryReply {
                term: details.current_term,
                success: false,
            }));
        } else if request.prev_index > details.commit_index {
            return Ok(Response::new(EntryReply {
                term: details.current_term,
                success: false,
            }));
        } else if request.commit_index > details.commit_index {
            let last_entry_index = match details.log.last() {
                Some(entry) => entry.0,
                None => 0,
            };
            details.commit_index = min(request.commit_index, last_entry_index);
        }

        Ok(Response::new(EntryReply {
            term: details.current_term,
            success: true,
        }))
    }

    async fn join(&self, request: Request<Byte>) -> Result<Response<Null>, Status> {
        if let Ok(str_addr) = String::from_utf8(request.into_inner().body) {
            // Ensure addr is a SocketAddr
            if let Ok(addr) = str_addr.parse::<SocketAddr>() {
                self.details.lock().await.cluster.push(format!("{}", addr));
                return Ok(Response::new(Null {}));
            }
        }

        Err(Status::internal("Unable to join"))
    }
}
