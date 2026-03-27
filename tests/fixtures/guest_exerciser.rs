use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

struct Config {
    duration_secs: u64,
    io_mib: usize,
    tcp_round_trips: usize,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let config = parse_args()?;
    let cpu = run_cpu_phase(config.duration_secs)?;
    let io = run_io_phase(config.io_mib)?;
    let tcp = run_tcp_phase(config.tcp_round_trips)?;
    println!(
        "{{\"status\":\"ok\",\"cpu_threads\":{},\"cpu_iterations\":{},\"io_bytes\":{},\"tcp_round_trips\":{}}}",
        cpu.threads, cpu.iterations, io.bytes, tcp.round_trips
    );
    Ok(())
}

fn parse_args() -> Result<Config, String> {
    let mut duration_secs = None;
    let mut io_mib = None;
    let mut tcp_round_trips = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {arg}"))?;
        match arg.as_str() {
            "--duration-secs" => {
                duration_secs = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| format!("invalid duration: {value}"))?,
                );
            }
            "--io-mib" => {
                io_mib = Some(
                    value
                        .parse::<usize>()
                        .map_err(|_| format!("invalid io size: {value}"))?,
                );
            }
            "--tcp-round-trips" => {
                tcp_round_trips = Some(
                    value
                        .parse::<usize>()
                        .map_err(|_| format!("invalid tcp round trips: {value}"))?,
                );
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    Ok(Config {
        duration_secs: duration_secs.ok_or_else(|| "missing --duration-secs".to_string())?,
        io_mib: io_mib.ok_or_else(|| "missing --io-mib".to_string())?,
        tcp_round_trips: tcp_round_trips
            .ok_or_else(|| "missing --tcp-round-trips".to_string())?,
    })
}

struct CpuSummary {
    threads: usize,
    iterations: u64,
}

fn run_cpu_phase(duration_secs: u64) -> Result<CpuSummary, String> {
    let threads = std::thread::available_parallelism()
        .map(|value| value.get().min(2))
        .unwrap_or(1);
    let deadline = Instant::now() + Duration::from_secs(duration_secs.max(1));
    let mut handles = Vec::with_capacity(threads);
    for index in 0..threads {
        handles.push(thread::spawn(move || {
            let mut state = 0x9E37_79B9_7F4A_7C15_u64 ^ index as u64;
            let mut iterations = 0_u64;
            while Instant::now() < deadline {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                state ^= state.rotate_left(17);
                iterations = iterations.saturating_add(1);
            }
            (iterations, state)
        }));
    }

    let mut total_iterations = 0_u64;
    let mut checksum = 0_u64;
    for handle in handles {
        let (iterations, state) = handle
            .join()
            .map_err(|_| "cpu worker thread panicked".to_string())?;
        total_iterations = total_iterations.saturating_add(iterations);
        checksum ^= state;
    }
    if total_iterations == 0 || checksum == 0 {
        return Err("cpu phase produced no useful work".to_string());
    }
    Ok(CpuSummary {
        threads,
        iterations: total_iterations,
    })
}

struct IoSummary {
    bytes: u64,
}

fn run_io_phase(io_mib: usize) -> Result<IoSummary, String> {
    let base_dir = PathBuf::from("/tmp/hardpass-e2e");
    fs::create_dir_all(&base_dir).map_err(|err| format!("create {}: {err}", base_dir.display()))?;
    let path = base_dir.join(format!("payload-{}", std::process::id()));
    let mut pattern = vec![0_u8; 1024 * 1024];
    for (index, byte) in pattern.iter_mut().enumerate() {
        *byte = (index % 251) as u8;
    }

    let expected_bytes = io_mib as u64 * pattern.len() as u64;
    let mut file = File::create(&path).map_err(|err| format!("create {}: {err}", path.display()))?;
    for _ in 0..io_mib {
        file.write_all(&pattern)
            .map_err(|err| format!("write {}: {err}", path.display()))?;
    }
    file.sync_all()
        .map_err(|err| format!("sync {}: {err}", path.display()))?;
    drop(file);

    let mut file = File::open(&path).map_err(|err| format!("open {}: {err}", path.display()))?;
    let mut actual_bytes = 0_u64;
    let mut buffer = vec![0_u8; pattern.len()];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|err| format!("read {}: {err}", path.display()))?;
        if read == 0 {
            break;
        }
        if buffer[..read] != pattern[..read] {
            return Err(format!("mismatched bytes while reading {}", path.display()));
        }
        actual_bytes = actual_bytes.saturating_add(read as u64);
    }
    fs::remove_file(&path).map_err(|err| format!("remove {}: {err}", path.display()))?;
    if actual_bytes != expected_bytes {
        return Err(format!(
            "expected {expected_bytes} bytes, read back {actual_bytes}"
        ));
    }
    Ok(IoSummary { bytes: actual_bytes })
}

struct TcpSummary {
    round_trips: usize,
}

fn run_tcp_phase(round_trips: usize) -> Result<TcpSummary, String> {
    let listener =
        TcpListener::bind(("127.0.0.1", 0)).map_err(|err| format!("bind localhost tcp: {err}"))?;
    let address = listener
        .local_addr()
        .map_err(|err| format!("read listener address: {err}"))?;
    let server = thread::spawn(move || -> Result<(), String> {
        let (mut stream, _) = listener.accept().map_err(|err| format!("accept: {err}"))?;
        let mut payload = [0_u8; 8];
        for _ in 0..round_trips {
            stream
                .read_exact(&mut payload)
                .map_err(|err| format!("server read_exact: {err}"))?;
            stream
                .write_all(&payload)
                .map_err(|err| format!("server write_all: {err}"))?;
        }
        Ok(())
    });

    let mut stream = TcpStream::connect(address).map_err(|err| format!("connect: {err}"))?;
    let payload = *b"hardpass";
    let mut echoed = [0_u8; 8];
    for _ in 0..round_trips {
        stream
            .write_all(&payload)
            .map_err(|err| format!("client write_all: {err}"))?;
        stream
            .read_exact(&mut echoed)
            .map_err(|err| format!("client read_exact: {err}"))?;
        if echoed != payload {
            return Err("tcp echo payload mismatch".to_string());
        }
    }

    server
        .join()
        .map_err(|_| "tcp server thread panicked".to_string())??;
    Ok(TcpSummary { round_trips })
}
