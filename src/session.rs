use crate::profile::Profile;
use crate::profile::StreamType;

use std::array::IntoIter;
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io;
use std::io::Read;

use std::path::Path;
use std::process::Stdio;

use std::time::Duration;
use std::time::Instant;

use std::fs::File;

use std::sync::Arc;
use std::sync::RwLock;

use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;

use tokio::process::Child;
use tokio::process::ChildStdout;
use tokio::process::Command;

use tokio::task::JoinHandle;

use tokio_stream::wrappers::LinesStream;
use tokio_stream::StreamExt;

/// Length of a chunk in seconds.
const CHUNK_SIZE: u32 = 5;
/// Represents how many chunks we encode before we require a timeout reset.
/// Basically if within MAX_CHUNKS_AHEAD we do not get a timeout reset we kill the stream.
/// This can be tuned
const MAX_CHUNKS_AHEAD: u32 = 15;

lazy_static::lazy_static! {
    /// This static contains stats about each stream. It is a Map of maps containing k/v pairs
    /// parsed from the ffmpeg stdout. Each Map is keyed by a session id.
    pub static ref STREAMING_SESSION: Arc<RwLock<HashMap<String, HashMap<String, String>>>> =
        Arc::new(RwLock::new(HashMap::new()));
}

pub struct Session {
    pub id: String,
    file: String,
    outdir: String,
    ffmpeg_bin: String,
    _process: Option<JoinHandle<()>>,

    has_started: bool,
    pub paused: bool,
    pub start_number: u32,
    stream_type: StreamType,
    last_chunk: u32,
    hard_timeout: Instant,

    child_pid: Option<u32>,
    real_process: Option<Child>,
}

impl Session {
    pub fn new(
        id: String,
        file: String,
        start_number: u32,
        outdir: String,
        stream_type: StreamType,
        ffmpeg_bin: String,
    ) -> Self {
        std::fs::create_dir_all(&outdir).unwrap();

        Self {
            id,
            outdir,
            ffmpeg_bin,
            stream_type,
            start_number,
            last_chunk: 0,
            _process: None,
            paused: false,
            has_started: false,
            child_pid: None,
            real_process: None,
            hard_timeout: Instant::now() + Duration::from_secs(30 * 60),
            file,
        }
    }

    pub async fn start(&mut self) -> Result<(), io::Error> {
        // make sure we actually have a path to write files to.
        self.has_started = true;
        self.paused = false;

        let _ = fs::create_dir_all(self.outdir.clone());
        let args = self.build_args();

        let log_file = format!("{}/ffmpeg.log", &self.outdir);

        let stderr: Stdio = File::create(log_file)?.into();
        let stdout: Stdio = if let StreamType::Subtitle { .. } = self.stream_type {
            File::create(format!("{}/stream.vtt", &self.outdir))?.into()
        } else {
            Stdio::piped()
        };

        let mut process = Command::new(self.ffmpeg_bin.clone())
            .stdout(stdout)
            .stderr(stderr)
            .args(args.as_slice())
            .spawn()?;

        self.child_pid = process.id();

        if !matches!(
            self.stream_type,
            StreamType::Subtitle { .. } | StreamType::RawVideo { .. }
        ) {
            let stdout = process.stdout.take().unwrap();
            let stdout_parser_thread =
                StdoutParser::new(self.id.clone(), stdout, self.child_pid.clone().unwrap());

            self.real_process = Some(process);

            self._process = Some(tokio::spawn(stdout_parser_thread.handle()));
        } else {
            self.real_process = Some(process);
        }

        Ok(())
    }

    // NOTE: This will only work for RawVideo streams.
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.real_process.as_mut().and_then(|x| x.stdout.take())
    }

    pub fn start_num(&self) -> u32 {
        self.start_number
    }

    fn build_args(&self) -> Vec<String> {
        let mut args = IntoIter::new(["-fflags", "+genpts", "-y"])
            .map(ToString::to_string)
            .collect::<Vec<_>>();

        if let StreamType::RawVideo { sseof, .. } = self.stream_type {
            if let Some(sseof) = sseof {
                args.append(&mut vec!["-sseof".into(), (-sseof).to_string()]);
            }
        } else {
            args.append(&mut vec![
                "-ss".into(),
                (self.start_num() * CHUNK_SIZE).to_string(),
            ])
        }
        args.append(&mut vec!["-i".into(), self.file.clone()]);

        match self.stream_type {
            StreamType::Audio { map, profile } => {
                args.append(&mut vec![
                    "-copyts".into(),
                    "-map".into(),
                    format!("0:{}", map),
                ]);
                args.append(&mut profile.to_args(self.start_num(), &self.outdir));
            }
            StreamType::Video { map, profile } => {
                args.append(&mut vec![
                    "-copyts".into(),
                    "-map".into(),
                    format!("0:{}", map),
                ]);
                args.append(&mut profile.to_args(self.start_num(), &self.outdir));
            }
            StreamType::Subtitle { map, profile } => {
                args.append(&mut vec!["-map".into(), format!("0:{}", map)]);

                args.append(&mut profile.to_args(0, &self.outdir));
            }
            StreamType::RawVideo {
                map,
                profile,
                tt,
                sseof,
            } => {
                args.append(&mut vec!["-map".into(), format!("0:{}", map)]);
                if sseof.is_none() {
                    if let Some(tt) = tt {
                        args.append(&mut vec!["-t".into(), tt.to_string()]);
                    }
                }
                args.append(&mut profile.to_args(0, &self.outdir));
            }
        }

        args
    }

    pub async fn join(&mut self) {
        if let Some(ref mut x) = self.real_process {
            let _ = x.kill().await;
            let _ = x.wait().await;
        }
    }

    pub fn stderr(&mut self) -> Option<String> {
        let file = format!("{}/ffmpeg.log", &self.outdir);

        let mut buf = String::new();
        let _ = File::open(file).ok()?.read_to_string(&mut buf);

        if buf.len() <= 1000 {
            return Some(buf);
        }

        Some(buf.split_off(buf.len() - 1000))
    }

    pub fn try_wait(&mut self) -> bool {
        if let Some(ref mut x) = self.real_process {
            if let Ok(Some(_)) = x.try_wait() {
                return true;
            }
        }

        false
    }

    pub fn is_hard_timeout(&mut self) -> bool {
        Instant::now() > self.hard_timeout
    }

    pub fn set_timeout(&mut self) {
        self.hard_timeout = Instant::now();
    }

    pub fn delete_tmp(&self) {
        let _ = fs::remove_dir_all(self.outdir.clone());
    }

    pub fn is_dead(&self) -> bool {
        if let Some(x) = self.child_pid {
            return crate::utils::is_process_effectively_dead(x);
        }

        true
    }

    pub fn pause(&mut self) {
        if let Some(x) = self.child_pid {
            if !self.paused {
                crate::utils::pause_proc(x as i32);
                self.paused = true;
            }
        }
    }

    pub fn cont(&mut self) {
        if let Some(x) = self.child_pid {
            if self.paused {
                crate::utils::cont_proc(x as i32);
                self.paused = false;
            }
        }
    }

    pub fn get_key(&self, k: &str) -> Option<String> {
        let session = STREAMING_SESSION.read().unwrap();
        session.get(&self.id)?.get(k).cloned()
    }

    pub fn current_chunk(&self) -> u32 {
        let frame = match self.stream_type {
            StreamType::Audio { .. } => {
                self.get_key("out_time_us")
                    .map(|x| x.parse::<u64>().unwrap_or(0))
                    .unwrap_or(0)
                    / 1000
                    / 1000
                    * 24
            }
            StreamType::Video { .. } => self
                .get_key("frame")
                .map(|x| x.parse::<u64>().unwrap_or(0))
                .unwrap_or(0),
            _ => 0,
        } as u32;

        match self.stream_type {
            StreamType::Audio { .. } => (frame / (CHUNK_SIZE * 24)).max(self.last_chunk),
            StreamType::Video { .. } => frame / (CHUNK_SIZE * 24) + self.start_number,
            _ => 0,
        }
    }

    pub fn raw_speed(&self) -> f64 {
        self.get_key("speed")
            .map(|x| x.trim_end_matches('x').to_string())
            .and_then(|x| x.parse::<f64>().ok())
            .unwrap_or(1.0) // assume if key is missing that our speed is 2.0
    }

    // returns how many chunks per second
    pub fn speed(&self) -> f64 {
        (self.raw_speed().floor().max(20.0) * 24.0) / (CHUNK_SIZE as f64 * 24.0)
    }

    pub fn eta_for(&self, chunk: u32) -> Duration {
        let cps = self.speed();

        let current_chunk = self.current_chunk() as f64;
        let diff = (chunk as f64 - current_chunk).abs();

        Duration::from_secs((diff / cps).abs().ceil() as u64)
    }

    /// Method does some math magic to guess if a chunk has been fully written by ffmpeg yet
    /// only works when `ffmpeg` writes files to tmp then renames them.
    pub fn is_chunk_done(&self, chunk_num: u32) -> bool {
        Path::new(&format!("{}/{}.m4s", &self.outdir, chunk_num)).is_file()
    }

    pub fn subtitle(&self, file: String) -> Option<String> {
        if !matches!(self.stream_type, StreamType::Subtitle { .. }) {
            return None;
        }

        let file = format!("{}/{}", &self.outdir, file);
        let path = Path::new(&file);

        // NOTE: This will not check if the ffmpeg process is dead, thus this will return immediately
        if path.is_file() {
            return path.to_str().map(ToString::to_string);
        }

        None
    }

    pub fn is_timeout(&self) -> bool {
        self.current_chunk() > self.last_chunk + MAX_CHUNKS_AHEAD
    }

    pub fn reset_timeout(&mut self, last_requested: u32) {
        self.last_chunk = last_requested;
        self.hard_timeout = Instant::now() + Duration::from_secs(30 * 60);
    }

    pub fn chunk_to_path(&self, chunk_num: u32) -> String {
        format!("{}/{}.m4s", self.outdir, chunk_num)
    }

    pub fn init_seg(&self) -> String {
        format!("{}/{}_init.mp4", self.outdir, self.start_num())
    }

    pub fn has_started(&self) -> bool {
        self.has_started
    }

    pub fn reset_to(&mut self, chunk: u32) {
        self.start_number = chunk;
        self._process = None;
        self.last_chunk = chunk;
        self.has_started = false;
        self.paused = true;
        self.child_pid = None;
    }
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("id", &self.id)
            .field("start_number", &self.start_number)
            .field("last_chunk", &self.last_chunk)
            .finish()
    }
}

struct StdoutParser {
    id: String,
    process_stdout: ChildStdout,
    pid: u32,
}

impl StdoutParser {
    fn new(id: String, process_stdout: ChildStdout, pid: u32) -> Self {
        Self {
            id,
            process_stdout,
            pid,
        }
    }

    async fn handle(self) {
        let mut stdio = LinesStream::new(BufReader::new(self.process_stdout).lines());
        let mut map: HashMap<String, String> = HashMap::new();

        let interval = tokio::time::interval(Duration::from_millis(100));
        tokio::pin!(interval);

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if crate::utils::is_process_effectively_dead(self.pid) {
                        break;
                    }
                },

                Some(Ok(v)) = stdio.next() => {
                    let output: Vec<&str> = v.split('=').collect();

                    // remove whitespace on both ends
                    map.insert(output[0].into(), output[1].trim_start().trim_end().into());

                    {
                        let mut lock = STREAMING_SESSION.write().unwrap();
                        let _ = lock.insert(self.id.clone(), map.clone());
                    }

                    continue;
                }
            }
        }

        let mut lock = STREAMING_SESSION.write().unwrap();
        let _ = lock.remove(&self.id);
    }
}
