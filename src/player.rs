use cpal::{
    self,
    traits::{DeviceTrait, EventLoopTrait, HostTrait},
    Format, SampleFormat, SampleRate, StreamData, UnknownTypeOutputBuffer,
};
use crossbeam_channel::{Receiver, Sender};
use ctrlc;
use pbr::ProgressBar;
use std::sync::{
    atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::Duration;

use crate::audio::{Audio, AudioSpec, Sample};

const PLAYBACK_SLEEP: Duration = Duration::from_millis(250);
const QUIT_FADE: Duration = Duration::from_secs(5);

/// Simple audio playback

pub fn play_audio<T>(spec: AudioSpec, stream: Receiver<Audio<T>>)
where
    T: Sample,
{
    let format = Format {
        channels: spec.channels,
        sample_rate: SampleRate(spec.sample_rate),
        data_type: SampleFormat::F32,
    };

    let stream_arc = Arc::new(Mutex::new(stream));
    let stream_arc_for_run = Arc::clone(&stream_arc);
    let audio_arc = Arc::new(Mutex::new(Audio::from_spec(&spec)));
    let audio_arc_for_run = Arc::clone(&audio_arc);
    let playback_position: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
    let playback_position_for_run = Arc::clone(&playback_position);
    let total_playback_position: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
    let total_playback_position_for_run = Arc::clone(&playback_position);
    let playback_complete_flag = Arc::new(AtomicBool::new(false));
    let playback_complete_flag_for_run = Arc::clone(&playback_complete_flag);

    let host = cpal::default_host();
    let event_loop = Arc::new(host.event_loop());
    let event_loop_arc_for_run = Arc::clone(&event_loop);
    let output_device = host
        .default_output_device()
        .expect("failed to get default output device");
    println!(
        "Using default output device: \"{}\"",
        output_device.name().unwrap()
    );

    let output_stream_id = event_loop
        .build_output_stream(&output_device, &format)
        .unwrap();

    event_loop.play_stream(output_stream_id.clone()).unwrap();

    launch_cpal_thread(
        event_loop_arc_for_run,
        playback_position_for_run,
        total_playback_position_for_run,
        playback_complete_flag_for_run,
        stream_arc_for_run,
        audio_arc_for_run,
    );

    wait_for_playback(
        total_playback_position,
        playback_complete_flag,
        audio_arc,
        event_loop,
        output_stream_id,
    );
}

fn playback_progress_bar() -> ProgressBar<std::io::Stdout> {
    let mut progress_bar = ProgressBar::new(100);
    progress_bar.show_speed = false;
    progress_bar.show_counter = false;
    progress_bar.tick_format("▁▂▃▄▅▆▇█▇▆▅▄▃");
    progress_bar
}

fn launch_cpal_thread<T, E>(
    event_loop: Arc<E>,
    playback_pos_arc: Arc<AtomicUsize>,
    total_playback_pos_arc: Arc<AtomicUsize>,
    playback_complete_arc: Arc<AtomicBool>,
    stream_arc: Arc<Mutex<Receiver<Audio<T>>>>,
    audio_arc: Arc<Mutex<Audio<T>>>,
) where
    T: Sample,
    E: EventLoopTrait + Send + Sync + 'static,
{
    thread::spawn(move || {
        event_loop.run(move |_stream_id, stream_data| {
            let mut buffer = match stream_data {
                Ok(res) => match res {
                    StreamData::Output {
                        buffer: UnknownTypeOutputBuffer::F32(buffer),
                    } => buffer,
                    _ => panic!("unexpected buffer type"),
                },
                Err(e) => {
                    panic!("failed to fetch get audio stream: {:?}", e);
                }
            };

            let mut audio = audio_arc.lock().unwrap();

            for buffer_interleaved_samples in buffer.chunks_mut(audio.spec.channels as usize) {
                let mut playback_pos = playback_pos_arc.fetch_add(1, Ordering::SeqCst);
                total_playback_pos_arc.fetch_add(1, Ordering::SeqCst);
                if playback_pos >= audio.data[0].len() {
                    match stream_arc.lock().unwrap().recv() {
                        Ok(new_audio) => {
                            *audio = new_audio;
                            playback_pos_arc.store(0, Ordering::SeqCst);
                            playback_pos = 0;
                        }
                        Err(e) => {
                            playback_complete_arc.store(true, Ordering::SeqCst);
                        }
                    }
                }
                for (dest, src_channel) in buffer_interleaved_samples.iter_mut().zip(&audio.data) {
                    match src_channel.get(playback_pos) {
                        Some(sample) => *dest = (*sample).into_f32(),
                        None => {
                            *dest = 0.0;
                        }
                    }
                }
            }
        });
    });
}

fn control_c_handler<T>(
    quit_counter: &Arc<AtomicU16>,
    total_playback_pos: &Arc<AtomicUsize>,
    audio_arc: &Arc<Mutex<Audio<T>>>,
) where
    T: Sample,
{
    if quit_counter.fetch_add(1, Ordering::SeqCst) > 0 {
        // If ctrl-c was received more than once, quit without fading out
        println!("\nExiting immediately");
        return;
    }
    println!("\nGot quit signal, fading out audio for {:#?}", QUIT_FADE);
    // nb fade doesnt work with streaming model yet
    let mut audio = audio_arc.lock().unwrap();
    let fade_out_start = audio.sample_to_duration(total_playback_pos.load(Ordering::SeqCst));
    audio.fade_out(fade_out_start, QUIT_FADE);
    drop(audio);
    let quit_counter_2 = Arc::clone(&quit_counter);
    thread::spawn(move || {
        thread::sleep(QUIT_FADE + Duration::from_millis(50));
        quit_counter_2.fetch_add(1, Ordering::SeqCst);
    });
}

fn wait_for_playback<T, E>(
    total_playback_pos: Arc<AtomicUsize>,
    playback_complete_flag: Arc<AtomicBool>,
    audio_arc: Arc<Mutex<Audio<T>>>,
    event_loop: Arc<E>,
    output_stream_id: <E>::StreamId,
) where
    T: Sample,
    E: EventLoopTrait,
{
    let samples_dur = audio_arc.lock().unwrap().data[0].len();

    // On early quit, fade out the sound before quitting
    let quit_counter = Arc::new(AtomicU16::new(0));
    let quit_counter_clone = Arc::clone(&quit_counter);
    let total_playback_pos_clone = Arc::clone(&total_playback_pos);
    ctrlc::set_handler(move || {
        control_c_handler(&quit_counter_clone, &total_playback_pos_clone, &audio_arc);
    })
    .unwrap();

    // Manage progress bar and wait for playback to complete
    let mut progress_bar = playback_progress_bar();
    loop {
        if playback_complete_flag.load(Ordering::SeqCst) {
            progress_bar.finish();
            println!("\nplayback complete");
            break;
        } else if quit_counter.load(Ordering::SeqCst) > 1 {
            progress_bar.finish();
            println!("\nplayback aborted");
            // need to explicitly exit with a non-zero exit code so the control-c quit
            // makes it to the shell so, for instance, bash loops can be broken.
            std::process::exit(1);
        }
        let current_playback_position = total_playback_pos.load(Ordering::SeqCst);
        progress_bar.set(((current_playback_position as f32 / samples_dur as f32) * 100.0) as u64);
        progress_bar.tick();
        thread::sleep(PLAYBACK_SLEEP);
    }
    event_loop.destroy_stream(output_stream_id);
}
