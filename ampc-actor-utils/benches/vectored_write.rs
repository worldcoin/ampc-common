use bytes::{Buf, BufMut, Bytes, BytesMut};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use std::collections::VecDeque;
use std::io::IoSlice;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
};

/// Buffers are already length-prefixed
#[derive(Clone)]
struct TestData {
    buffers: Vec<Bytes>,
}

impl TestData {
    fn new(num_messages: usize, message_size: usize) -> Self {
        let buffers: Vec<Bytes> = (0..num_messages)
            .map(|i| {
                // Length prefix + payload
                let mut data = Vec::with_capacity(4 + message_size);
                data.extend_from_slice(&(message_size as u32).to_le_bytes());
                data.extend((0..message_size).map(|j| ((i + j) % 256) as u8));
                Bytes::from(data)
            })
            .collect();
        Self { buffers }
    }
}

struct BatchNotify {
    num_messages: usize,
    done: oneshot::Sender<()>,
}

type BatchSender = mpsc::UnboundedSender<BatchNotify>;

/// Sets up a connected pair with in-process synchronization:
/// - Writer notifies reader via channel before writing
/// - Reader reads all messages, signals completion via oneshot
///
/// Returns (writer stream, batch sender).
async fn setup_connection() -> (TcpStream, BatchSender) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (batch_tx, mut batch_rx) = mpsc::unbounded_channel::<BatchNotify>();

    // Spawn reader that waits for batch notifications
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut len_buf = [0u8; 4];
        let mut payload_buf = vec![0u8; 64 * 1024];

        while let Some(batch) = batch_rx.recv().await {
            // Read each length-prefixed message
            for _ in 0..batch.num_messages {
                // Read 4-byte length prefix
                if stream.read_exact(&mut len_buf).await.is_err() {
                    return;
                }
                let payload_len = u32::from_le_bytes(len_buf) as usize;

                // Read payload
                let mut remaining = payload_len;
                while remaining > 0 {
                    let to_read = remaining.min(payload_buf.len());
                    match stream.read(&mut payload_buf[..to_read]).await {
                        Ok(0) => return,
                        Ok(n) => remaining -= n,
                        Err(_) => return,
                    }
                }
            }

            // Signal completion via oneshot
            let _ = batch.done.send(());
        }
    });

    let stream = TcpStream::connect(addr).await.unwrap();
    (stream, batch_tx)
}

/// Write using vectored I/O: batch up to 1024 IoSlices at a time
async fn do_write_vectored(
    stream: &mut TcpStream,
    done_rx: oneshot::Receiver<()>,
    buffers: &[Bytes],
) {
    let mut queue: VecDeque<Bytes> = buffers.iter().cloned().collect();

    while !queue.is_empty() {
        let mut iovs = [IoSlice::new(&[]); 1024];
        let n_iovs = queue
            .iter()
            .take(1024)
            .enumerate()
            .map(|(i, b)| {
                iovs[i] = IoSlice::new(b);
            })
            .count();
        let n = stream.write_vectored(&iovs[..n_iovs]).await.unwrap();

        let mut remaining_to_advance = n;
        while remaining_to_advance > 0 && !queue.is_empty() {
            let front = queue.front_mut().unwrap();
            let front_len = front.len();

            if front_len <= remaining_to_advance {
                remaining_to_advance -= front_len;
                queue.pop_front(); // Buffer fully sent
            } else {
                front.advance(remaining_to_advance); // Partial send
                remaining_to_advance = 0;
            }
        }
    }
    stream.flush().await.unwrap();
    done_rx.await.unwrap();
}

async fn do_write_single(
    stream: &mut TcpStream,
    done_rx: oneshot::Receiver<()>,
    buffers: &[Bytes],
) {
    let total_capacity: usize = buffers.iter().map(|b| b.len()).sum();
    let mut combined = BytesMut::with_capacity(total_capacity);
    for buf in buffers {
        combined.put(buf.as_ref());
    }
    stream.write_all(&combined.freeze()).await.unwrap();
    stream.flush().await.unwrap();
    done_rx.await.unwrap();
}

// (num_messages, message_size)
const CONFIGS: [(usize, usize); 9] = [
    (1, 1 << 10),
    (2, 1 << 10),
    (4, 1 << 10),
    (1, 100 * (1 << 10)),
    (2, 100 * (1 << 10)),
    (4, 100 * (1 << 10)),
    (1, 1 << 20),
    (2, 1 << 20),
    (4, 1 << 20),
];

fn bench_vectored_write(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("vectored_write");
    let mut temp = vec![0u8; 1 << 20];

    for (num_messages, message_size) in CONFIGS {
        let test_data = TestData::new(num_messages, message_size);
        let label = format!("{}x{}B", num_messages, message_size);
        let (mut stream, batch_tx) = rt.block_on(setup_connection());

        group.bench_function(BenchmarkId::new("vectored", &label), |b| {
            b.iter_batched(
                || {
                    let (done_tx, done_rx) = oneshot::channel();
                    batch_tx
                        .send(BatchNotify {
                            num_messages: test_data.buffers.len(),
                            done: done_tx,
                        })
                        .unwrap();
                    done_rx
                },
                |done_rx| {
                    // simulate serialization by one sender
                    temp.clear();
                    temp.extend_from_slice(&test_data.buffers[0]);
                    let _ = std::hint::black_box(&temp);
                    rt.block_on(do_write_vectored(&mut stream, done_rx, &test_data.buffers));
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

fn bench_single_write(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("single_write");

    for (num_messages, message_size) in CONFIGS {
        let test_data = TestData::new(num_messages, message_size);
        let label = format!("{}x{}B", num_messages, message_size);
        let (mut stream, batch_tx) = rt.block_on(setup_connection());

        group.bench_function(BenchmarkId::new("single", &label), |b| {
            b.iter_batched(
                || {
                    let (done_tx, done_rx) = oneshot::channel();
                    batch_tx
                        .send(BatchNotify {
                            num_messages: test_data.buffers.len(),
                            done: done_tx,
                        })
                        .unwrap();
                    done_rx
                },
                |done_rx| {
                    rt.block_on(do_write_single(&mut stream, done_rx, &test_data.buffers));
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

criterion_group!(benches, bench_vectored_write, bench_single_write);
criterion_main!(benches);
