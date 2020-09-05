use crate::audio::{Audio, AudioBus, AudioSpec};
use crate::player_processor::{AudioOutputProcessor, AudioOutputProcessorControlMessage};
use crate::recorder_processor::{RecorderProcessor, RecorderProcessorControlMessage};
use crate::signal_flow::node::{ControlMessage, Node, Processor, ProcessorState};
use crate::stretcher::Stretcher;
use crate::stretcher_processor::{StretcherProcessor, StretcherProcessorControlMessage};
use crate::windows;

use anyhow::Result;
use cpal::{
    self,
    traits::{DeviceTrait, EventLoopTrait, HostTrait},
    SampleFormat, SampleRate, StreamData, UnknownTypeInputBuffer,
};
use crossbeam_channel::{unbounded, Receiver, RecvError, Sender, TryRecvError};
use rand::{self, Rng};
use slice_deque::SliceDeque;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[derive(Debug)]
pub enum InstallationProcessorControlMessage {
    Shutdown,
}

impl ControlMessage for InstallationProcessorControlMessage {
    fn shutdown_msg() -> Self {
        InstallationProcessorControlMessage::Shutdown
    }
}

pub struct InstallationProcessorConfig {
    pub spec: AudioSpec,
    pub max_stretchers: u8,
    pub max_snippet_dur: Duration,
    pub ambient_volume_window_dur: Duration,
    pub current_volume_window_dur: Duration,
    pub amp_activation_factor: f32,
    pub window_sizes: Vec<usize>,
    pub min_stretch_factor: f32,
    pub max_stretch_factor: f32,
}

impl Default for InstallationProcessorConfig {
    fn default() -> Self {
        InstallationProcessorConfig {
            spec: AudioSpec {
                channels: 2,
                sample_rate: 44100,
            },
            max_stretchers: 10,
            max_snippet_dur: Duration::from_secs(1),
            ambient_volume_window_dur: Duration::from_secs(10),
            current_volume_window_dur: Duration::from_millis(300),
            amp_activation_factor: 1.5,
            window_sizes: vec![8192],
            min_stretch_factor: 6.0,
            max_stretch_factor: 12.0,
        }
    }
}

pub struct InstallationProcessor {
    config: InstallationProcessorConfig,
}

#[derive(Debug, Copy, Clone)]
enum ListeningState {
    Idle,
    Active,
}

impl InstallationProcessor {
    pub fn new(config: InstallationProcessorConfig) -> Self {
        InstallationProcessor { config }
    }

    fn run(mut self, ctrl_rx: Receiver<InstallationProcessorControlMessage>) -> Result<()> {
        let spec = self.config.spec;
        let (recorder_processor, recorder_bus) = RecorderProcessor::new(spec);
        let recorder = Node::new(recorder_processor);
        let player = Node::new(AudioOutputProcessor::new(spec));

        let mut stretcher_nodes = vec![];

        const rec_buf_chunks: usize = 1024;
        let ambient_amp_window_size = (self.config.ambient_volume_window_dur.as_secs_f32()
            * spec.sample_rate as f32) as usize
            * spec.channels as usize;
        let current_amp_window_size = (self.config.current_volume_window_dur.as_secs_f32()
            * spec.sample_rate as f32) as usize
            * spec.channels as usize;
        let mut ambient_amplitude: f32 = 0.0;
        let mut current_amplitude: f32 = 0.0;
        let mut recording_buffers: Vec<SliceDeque<Vec<f32>>> = (0..recorder_bus.channels.len())
            .map(|_| SliceDeque::with_capacity(rec_buf_chunks))
            .collect();
        let mut listening_state = ListeningState::Idle;
        let mut recording_buffer_listen_start: isize = 0;

        loop {
            // Fetch latest data from recorder
            let mut truncated_rec_bufs = false;
            recorder_bus.channels.iter().enumerate().for_each(
                |(i, channel_recv)| match channel_recv.recv() {
                    Ok(chunk) => {
                        let recording_buffer = unsafe { recording_buffers.get_unchecked_mut(i) };
                        if recording_buffer.len() == rec_buf_chunks {
                            truncated_rec_bufs = true;
                            recording_buffer.truncate_front(rec_buf_chunks - 1);
                        }
                        recording_buffer.push_back(chunk);
                    }
                    Err(RecvError) => panic!("recorder unexpectedly crashed"),
                },
            );
            if truncated_rec_bufs {
                recording_buffer_listen_start -= 1;
            }

            // Adjust the moving average amplitudes for ambient and current levels
            // new average = old average * (n-len(M))/n + (sum of values in M)/n).
            ambient_amplitude = Self::chunked_moving_average_amp(
                ambient_amplitude,
                ambient_amp_window_size,
                &recording_buffers,
            );
            current_amplitude = Self::chunked_moving_average_amp(
                current_amplitude,
                current_amp_window_size,
                &recording_buffers,
            );

            // todo this thresholding currently takes a flawed naive linear approach,
            // to work well it probably needs to be made exponential
            match listening_state {
                ListeningState::Idle => {
                    if recording_buffers[0].len() > rec_buf_chunks / 2
                        && current_amplitude > ambient_amplitude * self.config.amp_activation_factor
                    {
                        info!(
                            "Heard something, starting to listen. amp={}, ambient amp={}",
                            current_amplitude, ambient_amplitude
                        );
                        listening_state = ListeningState::Active;
                        recording_buffer_listen_start = recording_buffers[0].len() as isize;
                    }
                }
                ListeningState::Active => {
                    // Our "listening" audio has completely filled the recording buffer
                    // or the audio level has dropped below our threshold
                    if recording_buffer_listen_start == 0
                        || current_amplitude < ambient_amplitude / self.config.amp_activation_factor
                    {
                        info!(
                            "Event ended, playing back. amp={}, ambient amp={}",
                            current_amplitude, ambient_amplitude
                        );
                        listening_state = ListeningState::Idle;
                        let mut total_input_samples = 0;
                        let stretch_factor = self.choose_stretch_factor();
                        let window = self.choose_window();
                        let stretchers: Vec<Stretcher> = recording_buffers
                            .iter()
                            .map(|b| {
                                total_input_samples = 0;
                                let input_chunks = &b[recording_buffer_listen_start as usize..];
                                let (tx, rx) = unbounded();
                                input_chunks.iter().for_each(|chunk| {
                                    // could optimize this since we unecessarily count the samples once for every channel.
                                    total_input_samples += chunk.len();
                                    tx.send(chunk.clone()).unwrap();
                                });
                                return Stretcher::new(
                                    spec,
                                    rx,
                                    stretch_factor,
                                    1.0,
                                    1,
                                    window.clone(),
                                    Duration::from_secs(4),
                                    None,
                                );
                            })
                            .collect();
                        let (processor, bus) = StretcherProcessor::new(
                            stretchers,
                            Some((total_input_samples as f32 * stretch_factor) as usize),
                        );
                        stretcher_nodes.push(Node::new(processor));
                        player.send_control_message(
                            AudioOutputProcessorControlMessage::ConnectBus {
                                id: rand::thread_rng().gen(),
                                bus: bus,
                                fade: Some(Duration::from_millis(500)),
                                shutdown_when_finished: false,
                            },
                        );
                    }
                }
            }

            match self.handle_control_messages(&ctrl_rx)? {
                ProcessorState::Finished => {
                    break;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn choose_window(&self) -> Vec<f32> {
        let size = self.config.window_sizes
            [rand::thread_rng().gen_range(0, self.config.window_sizes.len())];
        return windows::hanning(size);
    }

    fn choose_stretch_factor(&self) -> f32 {
        return rand::thread_rng().gen_range(
            self.config.min_stretch_factor,
            self.config.max_stretch_factor,
        );
    }

    fn chunked_moving_average_amp(
        last_avg: f32,
        window_size: usize,
        recording_buffers: &Vec<SliceDeque<Vec<f32>>>,
    ) -> f32 {
        let last_chunk_len = recording_buffers[0].back().unwrap().len() * recording_buffers.len();
        (last_avg * ((window_size - last_chunk_len) as f32 / window_size as f32))
            + (recording_buffers
                .iter()
                .map(|chunks| {
                    chunks
                        .back()
                        .unwrap()
                        .iter()
                        .map(|sample| sample.abs())
                        .sum::<f32>()
                })
                .sum::<f32>() as f32
                / window_size as f32)
    }
}

impl Processor<InstallationProcessorControlMessage> for InstallationProcessor {
    fn handle_control_messages(
        &mut self,
        rx: &Receiver<InstallationProcessorControlMessage>,
    ) -> Result<ProcessorState> {
        match rx.try_recv() {
            Ok(msg) => match msg {
                InstallationProcessorControlMessage::Shutdown => Ok(ProcessorState::Finished),
            },
            Err(TryRecvError::Disconnected) => Ok(ProcessorState::Finished),
            Err(TryRecvError::Empty) => Ok(ProcessorState::Running),
        }
    }

    fn start(
        self,
        finished: Arc<AtomicBool>,
    ) -> (Sender<InstallationProcessorControlMessage>, JoinHandle<()>) {
        let (ctrl_tx, ctrl_rx) = unbounded();
        let handle = thread::spawn(move || {
            self.run(ctrl_rx).unwrap();
            finished.store(true, Ordering::Relaxed);
        });
        (ctrl_tx, handle)
    }
}
