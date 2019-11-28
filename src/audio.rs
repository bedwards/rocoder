use num_traits::Num;
use std::time::Duration;

#[derive(Copy, Clone, Debug)]
pub struct AudioSpec {
    /// Number of audio channels (e.g. 2 for stereo)
    pub channels: u16,
    /// Number of samples per second
    pub sample_rate: u32,
}

pub struct Audio<T>
where
    T: Sized + Num + Copy,
{
    pub data: Vec<Vec<T>>,
    pub spec: AudioSpec,
}

impl<T> Audio<T>
where
    T: Sized + Num + Copy,
{
    pub fn clip_in_place(&mut self, start_offset: Option<Duration>, duration: Option<Duration>) {
        let start_sample_pos = self.resolve_start_sample_pos(start_offset);
        let end_sample_pos = self.resolve_end_sample_pos(start_sample_pos, duration);
        for channel in self.data.iter_mut() {
            *channel = channel[start_sample_pos..end_sample_pos].to_vec();
        }
    }

    fn resolve_start_sample_pos(&self, start_offset: Option<Duration>) -> usize {
        match start_offset {
            Some(offset) => (offset.as_secs_f64() * self.spec.sample_rate as f64) as usize,
            None => 0,
        }
    }

    fn resolve_end_sample_pos(&self, start_sample_pos: usize, duration: Option<Duration>) -> usize {
        match duration {
            Some(dur) => {
                let dur_in_samples = (dur.as_secs_f64() * self.spec.sample_rate as f64) as usize;
                start_sample_pos + dur_in_samples
            }
            None => self.data.get(0).unwrap().len(),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_clip_in_place_both_args_none() {
        let mut audio = internal_audio(5, 2, 2);
        audio.clip_in_place(None, None);
        assert_eq!(audio.data.get(0).unwrap().len(), 5);
        assert_eq!(audio.data.get(1).unwrap().len(), 5);
    }

    #[test]
    fn test_clip_in_place_only_start_offset_given() {
        let mut audio = internal_audio(5, 2, 2);
        audio.clip_in_place(Some(Duration::from_millis(500)), None);
        assert_eq!(audio.data.get(0).unwrap().len(), 4);
        assert_eq!(audio.data.get(1).unwrap().len(), 4);
    }

    #[test]
    fn test_clip_in_place_only_duration_given() {
        let mut audio = internal_audio(5, 2, 2);
        audio.clip_in_place(None, Some(Duration::from_millis(500)));
        assert_eq!(audio.data.get(0).unwrap().len(), 1);
        assert_eq!(audio.data.get(1).unwrap().len(), 1);
    }

    #[test]
    fn test_clip_in_place_both_given() {
        let mut audio = internal_audio(5, 2, 2);
        audio.clip_in_place(
            Some(Duration::from_millis(500)),
            Some(Duration::from_millis(1000)),
        );
        assert_eq!(audio.data.get(0).unwrap().len(), 2);
        assert_eq!(audio.data.get(1).unwrap().len(), 2);
    }

    fn internal_audio(len: usize, channels: u16, sample_rate: u32) -> Audio<f32> {
        let mut data: Vec<Vec<f32>> = Vec::new();
        for _ in 0..channels {
            data.push(vec![0.0; len]);
        }
        Audio {
            data,
            spec: AudioSpec {
                channels,
                sample_rate,
            },
        }
    }
}