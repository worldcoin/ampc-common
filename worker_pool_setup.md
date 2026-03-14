
The worker pool networking needs an API that provides two mechanisms:
- Leader networking
  - external: leader handle (a struct that wraps the channels)
  - manages a task via channels
- Worker networking
  - same pattern as the leadfer

# Leader
- is given a listen address, an id, and a list of worker SocketAddr
- spawns the leader task. this task connects to each worker, receives jobs via a channel, does scatter/gather, then returns the result
- leader handle just submits jobs via a channel. the job has a response channel, so that multiple tasks can use the same handle

# worker
- is given a listen address, the leader id, and the leader SocketAddr.
- spawns the worker task, which connects to the leader, receives jobs, forwards them via the channel, receives responses, and sends them back
- the worker handle will be used by the application to receive and process jobs.

in both cases, the worker/leader are just used for transport. the application will have to frame and parse its own messages.

# code

pub type Payload = Vec<u8>;

# Leader Handle
#[derive(Clone)]
struct LeaderHandle {
  ct: CancellationToken,
  ch: UnboundedSender<Job>,
  num_workers: usize
}

impl LeaderHandle {
    /// Broadcast to all workers, wait for all acks
    async fn broadcast(&self, payload: Payload) -> Result<Vec<WorkerMsg>>{
      let (tx, rx) = oneshot::channel();
      self.ch.send(Job::Broadcast{payload, rsp: tx});
      rx.await?
    }

    /// Scatter-gather: send different payloads to different workers, gather results in partition order
    async fn scatter_gather(&self, msgs: Vec<WorkerMsg>) -> Result<Vec<WorkerMsg>> {
      let (tx, rx) = oneshot::channel();
      self.ch.send(Job::ScatterGather{msgs, rsp: tx});
      rx.await?
    }

    fn num_workers(&self) -> usize {
      self.num_workers
    }
}

pub struct Leader<T: NetworkConnection + 'static, C: Client<Output = T> + 'static> {
    my_id: Arc<Identity>,
    workers: Vec<Arc<Peer>>,
    connector: C,
    conn_cmd_tx: UnboundedSender<ConnectionRequest<T>>,
    connection_state: ConnectionState,
}

impl<T: NetworkConnection + 'static, C: Client<Output = T> + 'static> Leader<T,C> {
  // todo
}

pub struct WorkerMsg {
    worker_id: u16,
    payload: Payload,
}

enum Job {
  Broadcast {
    payload: Payload,
    rsp: oneshot::Sender<Result<Vec<WorkerMsg>, _>>
  },
  ScatterGather{
    msgs: Vec<WorkerMsg>,
    rsp: oneshot::Sender<Result<Vec<WorkerMsg>, _>>
  }
}

# Worker Handle

struct WorkerHandle {}
impl WorkerHandle {
  async fn recv(&self) -> Job;
}

pub struct Msg {
  job_id: u32,
  worker_id: u16,
  payload: Payload,
}

pub struct Job {
  msg: Msg,
  rsp: oneshot::channel<Result<Msg, _>>
}

impl Job {
  pub fn send_result(self, payload: Payload) {
    self.rsp.send(Msg{job_id: self.msg.job_id, worker_id: self.msg.worker_id, payload})
  }
}


# connection establishment
probably want to use N connections but will hold off on that for now. if network becomes a bottleneck then we can add more.

# background tasks

## Leader task
the leader task will connect N times (N=1 for now) to each worker. for each connection, it will spawn a task that will pass the NetworkConnection trait type to tokio::io::split and then select on the read and write halves and a cancellation token. it will receive messages over a channel from the Leader struct and write them to the write half and also receive any messages over the read half and forward them back to the leader struct.

the leader struct will eventually track completions and emit completed jobs back to the response channel, which is part of the Job enum

the connection manager tasks should handle reconnection. the connection logic should not be in the Leader struct. the Leader struct will use a command channel with a variant to get the connection status of each worker. 

## worker task
the worker task will spawn a task to manage the connection to the leader. the worker task will handle reconnections, and be controlled by a command channel


