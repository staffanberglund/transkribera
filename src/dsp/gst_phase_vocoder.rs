use std::sync::{
    Mutex, MutexGuard, OnceLock,
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
};

use anyhow::{Context, Result, bail};
use gst::{glib, prelude::*, subclass::prelude::*};
use gstreamer as gst;

use super::processor::{
    MAX_PLAYBACK_SPEED, MIN_PLAYBACK_SPEED, TempoProcessor, TempoProcessorConfig,
    create_tempo_processor,
};

const UNKNOWN_POSITION_NS: u64 = u64::MAX;

struct StreamState {
    processor: Option<Box<dyn TempoProcessor>>,
    sample_rate: u32,
    channels: usize,
    input_samples: Vec<f32>,
    output_samples: Vec<f32>,
    next_output_pts: Option<gst::ClockTime>,
    mark_discontinuity: bool,
    bypass_active: bool,
}

impl Default for StreamState {
    fn default() -> Self {
        Self {
            processor: None,
            sample_rate: 0,
            channels: 0,
            input_samples: Vec::new(),
            output_samples: Vec::new(),
            next_output_pts: None,
            mark_discontinuity: true,
            bypass_active: false,
        }
    }
}

impl StreamState {
    fn configure(&mut self, caps: &gst::CapsRef, playback_speed: f32, bypass: bool) -> Result<()> {
        let structure = caps.structure(0).context("audio caps have no structure")?;
        let format = structure
            .get::<String>("format")
            .context("audio caps have no sample format")?;
        if format != "F32LE" {
            bail!("phase vocoder requires F32LE audio, received {format}");
        }
        let layout = structure
            .get::<String>("layout")
            .context("audio caps have no layout")?;
        if layout != "interleaved" {
            bail!("phase vocoder requires interleaved audio, received {layout}");
        }
        let sample_rate = structure
            .get::<i32>("rate")
            .context("audio caps have no sample rate")?;
        let channels = structure
            .get::<i32>("channels")
            .context("audio caps have no channel count")?;
        let sample_rate = u32::try_from(sample_rate).context("sample rate is negative")?;
        let channels = usize::try_from(channels).context("channel count is negative")?;
        self.processor = Some(create_tempo_processor(TempoProcessorConfig {
            sample_rate,
            channel_count: channels,
            playback_speed,
        })?);
        self.sample_rate = sample_rate;
        self.channels = channels;
        self.input_samples.clear();
        self.output_samples.clear();
        self.next_output_pts = None;
        self.mark_discontinuity = true;
        self.bypass_active = bypass;
        Ok(())
    }

    fn set_bypass_mode(&mut self, bypass: bool, output_start: Option<gst::ClockTime>) {
        if self.bypass_active != bypass {
            self.reset(output_start);
            self.bypass_active = bypass;
        }
    }

    fn reset(&mut self, output_start: Option<gst::ClockTime>) {
        if let Some(processor) = &mut self.processor {
            processor.reset();
        }
        self.input_samples.clear();
        self.output_samples.clear();
        self.next_output_pts = output_start;
        self.mark_discontinuity = true;
    }

    fn process_buffer(
        &mut self,
        buffer: &gst::BufferRef,
        playback_speed: f32,
    ) -> Result<Option<gst::Buffer>> {
        if buffer.flags().contains(gst::BufferFlags::DISCONT) {
            self.reset(buffer.pts());
        }
        let map = buffer
            .map_readable()
            .map_err(|_| anyhow::anyhow!("input audio buffer is not readable"))?;
        let bytes = map.as_slice();
        if bytes.len() % std::mem::size_of::<f32>() != 0 {
            bail!("input audio buffer does not contain complete f32 samples");
        }
        self.input_samples.clear();
        self.input_samples.reserve(bytes.len() / 4);
        self.input_samples.extend(
            bytes
                .chunks_exact(4)
                .map(|sample| f32::from_le_bytes([sample[0], sample[1], sample[2], sample[3]])),
        );
        if self.channels == 0 || !self.input_samples.len().is_multiple_of(self.channels) {
            bail!("input audio buffer does not contain complete interleaved frames");
        }

        let processor = self
            .processor
            .as_mut()
            .context("tempo processor received audio before caps")?;
        processor.set_playback_speed(playback_speed)?;
        self.output_samples.clear();
        processor.process(&self.input_samples, &mut self.output_samples)?;
        if self.next_output_pts.is_none() {
            self.next_output_pts = buffer.pts();
        }
        self.take_output_buffer()
    }

    fn flush(&mut self) -> Result<Option<gst::Buffer>> {
        if self.bypass_active {
            return Ok(None);
        }
        let Some(processor) = &mut self.processor else {
            return Ok(None);
        };
        self.output_samples.clear();
        processor.flush(&mut self.output_samples)?;
        self.take_output_buffer()
    }

    fn take_output_buffer(&mut self) -> Result<Option<gst::Buffer>> {
        if self.output_samples.is_empty() {
            return Ok(None);
        }
        if self.channels == 0 || self.sample_rate == 0 {
            bail!("phase vocoder output format is not configured");
        }
        let frames = self.output_samples.len() / self.channels;
        let duration_ns = ((frames as u128 * 1_000_000_000_u128) / self.sample_rate as u128)
            .min(u64::MAX as u128) as u64;
        let duration = gst::ClockTime::from_nseconds(duration_ns);
        let pts = self.next_output_pts;

        let mut bytes = Vec::with_capacity(self.output_samples.len() * 4);
        for sample in &self.output_samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        let mut buffer = gst::Buffer::from_mut_slice(bytes);
        if let Some(buffer) = buffer.get_mut() {
            buffer.set_pts(pts);
            buffer.set_duration(duration);
            if self.mark_discontinuity {
                buffer.set_flags(gst::BufferFlags::DISCONT);
                self.mark_discontinuity = false;
            }
        }
        self.next_output_pts = pts.map(|pts| pts.saturating_add(duration));
        Ok(Some(buffer))
    }
}

mod imp {
    use super::*;

    pub struct GstPhaseVocoder {
        pub(super) srcpad: gst::Pad,
        pub(super) sinkpad: gst::Pad,
        pub(super) playback_speed_bits: AtomicU32,
        pub(super) bypass: AtomicBool,
        pub(super) source_position_ns: AtomicU64,
        state: Mutex<StreamState>,
    }

    impl GstPhaseVocoder {
        fn state(&self) -> MutexGuard<'_, StreamState> {
            self.state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        }

        fn playback_speed(&self) -> f32 {
            f32::from_bits(self.playback_speed_bits.load(Ordering::Relaxed))
        }

        fn bypass(&self) -> bool {
            self.bypass.load(Ordering::Relaxed)
        }

        fn post_processing_error(&self, error: &anyhow::Error) {
            gst::element_error!(
                self.obj(),
                gst::StreamError::Failed,
                ("Phase-vocoder processing failed"),
                ["{error:#}"]
            );
        }

        fn sink_chain(
            &self,
            _pad: &gst::Pad,
            buffer: gst::Buffer,
        ) -> Result<gst::FlowSuccess, gst::FlowError> {
            if let Some(pts) = buffer.pts() {
                let end_ns = buffer.duration().map_or(pts.nseconds(), |duration| {
                    pts.nseconds().saturating_add(duration.nseconds())
                });
                self.source_position_ns.store(end_ns, Ordering::Relaxed);
            }
            if self.bypass() {
                self.state().set_bypass_mode(true, buffer.pts());
                return self.srcpad.push(buffer);
            }
            self.state().set_bypass_mode(false, buffer.pts());
            let output = self
                .state()
                .process_buffer(buffer.as_ref(), self.playback_speed());
            match output {
                Ok(Some(output)) => self.srcpad.push(output),
                Ok(None) => Ok(gst::FlowSuccess::Ok),
                Err(error) => {
                    self.post_processing_error(&error);
                    Err(gst::FlowError::Error)
                }
            }
        }

        fn sink_event(&self, _pad: &gst::Pad, event: gst::Event) -> bool {
            use gst::EventView;

            match event.view() {
                EventView::Caps(caps_event) => {
                    if let Err(error) = self.state().configure(
                        caps_event.caps(),
                        self.playback_speed(),
                        self.bypass(),
                    ) {
                        self.post_processing_error(&error);
                        return false;
                    }
                    self.source_position_ns
                        .store(UNKNOWN_POSITION_NS, Ordering::Relaxed);
                    self.srcpad.push_event(event)
                }
                EventView::Segment(segment_event) => {
                    let Some(segment) = segment_event.segment().downcast_ref::<gst::ClockTime>()
                    else {
                        let error = anyhow::anyhow!("phase vocoder requires a time segment");
                        self.post_processing_error(&error);
                        return false;
                    };
                    let start = segment.start();
                    self.state().reset(start);
                    self.source_position_ns.store(
                        start.map_or(UNKNOWN_POSITION_NS, |time| time.nseconds()),
                        Ordering::Relaxed,
                    );
                    let mut output_segment = segment.clone();
                    output_segment.set_stop(gst::ClockTime::NONE);
                    self.srcpad
                        .push_event(gst::event::Segment::new(&output_segment))
                }
                EventView::FlushStart(_) | EventView::FlushStop(_) => {
                    self.state().reset(None);
                    self.source_position_ns
                        .store(UNKNOWN_POSITION_NS, Ordering::Relaxed);
                    self.srcpad.push_event(event)
                }
                EventView::Eos(_) => {
                    let output = self.state().flush();
                    match output {
                        Ok(Some(output)) => {
                            self.srcpad.push(output).is_ok() && self.srcpad.push_event(event)
                        }
                        Ok(None) => self.srcpad.push_event(event),
                        Err(error) => {
                            self.post_processing_error(&error);
                            false
                        }
                    }
                }
                _ => self.srcpad.push_event(event),
            }
        }

        fn sink_query(&self, _pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
            self.srcpad.peer_query(query)
        }

        fn src_event(&self, _pad: &gst::Pad, event: gst::Event) -> bool {
            self.sinkpad.push_event(event)
        }

        fn src_query(&self, _pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
            if !self.sinkpad.peer_query(query) {
                return false;
            }
            if let gst::QueryViewMut::Latency(latency) = query.view_mut() {
                let (live, minimum, maximum) = latency.result();
                let state = self.state();
                if !self.bypass()
                    && let Some(processor) = &state.processor
                {
                    let added_ns = (processor.latency_frames() as u128 * 1_000_000_000_u128
                        / processor.sample_rate() as u128)
                        .min(u64::MAX as u128) as u64;
                    let added = gst::ClockTime::from_nseconds(added_ns);
                    latency.set(
                        live,
                        minimum.saturating_add(added),
                        maximum.map(|maximum| maximum.saturating_add(added)),
                    );
                }
            }
            true
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for GstPhaseVocoder {
        const NAME: &'static str = "TranskriberaPhaseVocoder";
        type Type = super::GstPhaseVocoder;
        type ParentType = gst::Element;

        fn with_class(klass: &Self::Class) -> Self {
            let sink_template = klass
                .pad_template("sink")
                .unwrap_or_else(|| unreachable!("sink pad template is registered"));
            let sinkpad = gst::Pad::builder_from_template(&sink_template)
                .chain_function(|pad, parent, buffer| {
                    GstPhaseVocoder::catch_panic_pad_function(
                        parent,
                        || Err(gst::FlowError::Error),
                        |element| element.sink_chain(pad, buffer),
                    )
                })
                .event_function(|pad, parent, event| {
                    GstPhaseVocoder::catch_panic_pad_function(
                        parent,
                        || false,
                        |element| element.sink_event(pad, event),
                    )
                })
                .query_function(|pad, parent, query| {
                    GstPhaseVocoder::catch_panic_pad_function(
                        parent,
                        || false,
                        |element| element.sink_query(pad, query),
                    )
                })
                .build();

            let src_template = klass
                .pad_template("src")
                .unwrap_or_else(|| unreachable!("src pad template is registered"));
            let srcpad = gst::Pad::builder_from_template(&src_template)
                .event_function(|pad, parent, event| {
                    GstPhaseVocoder::catch_panic_pad_function(
                        parent,
                        || false,
                        |element| element.src_event(pad, event),
                    )
                })
                .query_function(|pad, parent, query| {
                    GstPhaseVocoder::catch_panic_pad_function(
                        parent,
                        || false,
                        |element| element.src_query(pad, query),
                    )
                })
                .build();

            Self {
                srcpad,
                sinkpad,
                playback_speed_bits: AtomicU32::new(1.0_f32.to_bits()),
                bypass: AtomicBool::new(false),
                source_position_ns: AtomicU64::new(UNKNOWN_POSITION_NS),
                state: Mutex::new(StreamState::default()),
            }
        }
    }

    impl ObjectImpl for GstPhaseVocoder {
        fn constructed(&self) {
            self.parent_constructed();
            let element = self.obj();
            if let Err(error) = element.add_pad(&self.sinkpad) {
                gst::element_error!(element, gst::CoreError::Failed, ["{error}"]);
            }
            if let Err(error) = element.add_pad(&self.srcpad) {
                gst::element_error!(element, gst::CoreError::Failed, ["{error}"]);
            }
        }

        fn properties() -> &'static [glib::ParamSpec] {
            static PROPERTIES: OnceLock<Vec<glib::ParamSpec>> = OnceLock::new();
            PROPERTIES.get_or_init(|| {
                vec![
                    glib::ParamSpecFloat::builder("playback-speed")
                        .nick("Playback speed")
                        .blurb("Output tempo divided by source tempo")
                        .minimum(MIN_PLAYBACK_SPEED)
                        .maximum(MAX_PLAYBACK_SPEED)
                        .default_value(1.0)
                        .build(),
                    glib::ParamSpecBoolean::builder("bypass")
                        .nick("Bypass")
                        .blurb("Forward decoded audio without tempo processing")
                        .default_value(false)
                        .build(),
                ]
            })
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            match pspec.name() {
                "playback-speed" => {
                    if let Ok(speed) = value.get::<f32>() {
                        self.playback_speed_bits.store(
                            speed
                                .clamp(MIN_PLAYBACK_SPEED, MAX_PLAYBACK_SPEED)
                                .to_bits(),
                            Ordering::Relaxed,
                        );
                    }
                }
                "bypass" => {
                    if let Ok(bypass) = value.get::<bool>() {
                        self.bypass.store(bypass, Ordering::Relaxed);
                    }
                }
                _ => unreachable!("unknown phase-vocoder property {}", pspec.name()),
            }
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "playback-speed" => self.playback_speed().to_value(),
                "bypass" => self.bypass().to_value(),
                _ => unreachable!("unknown phase-vocoder property {}", pspec.name()),
            }
        }
    }

    impl GstObjectImpl for GstPhaseVocoder {}

    impl ElementImpl for GstPhaseVocoder {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static METADATA: OnceLock<gst::subclass::ElementMetadata> = OnceLock::new();
            Some(METADATA.get_or_init(|| {
                gst::subclass::ElementMetadata::new(
                    "Rust phase vocoder",
                    "Filter/Effect/Audio",
                    "Changes audio tempo while preserving pitch",
                    "Staffan Berglund and Transkribera contributors",
                )
            }))
        }

        fn pad_templates() -> &'static [gst::PadTemplate] {
            static TEMPLATES: OnceLock<Vec<gst::PadTemplate>> = OnceLock::new();
            TEMPLATES.get_or_init(|| {
                let caps = gst::Caps::builder("audio/x-raw")
                    .field("format", "F32LE")
                    .field("layout", "interleaved")
                    .field("rate", gst::IntRange::<i32>::new(8_000, 192_000))
                    .field("channels", gst::IntRange::<i32>::new(1, 2))
                    .build();
                [gst::PadDirection::Sink, gst::PadDirection::Src]
                    .into_iter()
                    .map(|direction| {
                        let name = if direction == gst::PadDirection::Sink {
                            "sink"
                        } else {
                            "src"
                        };
                        gst::PadTemplate::new(name, direction, gst::PadPresence::Always, &caps)
                            .unwrap_or_else(|_| unreachable!("static phase-vocoder caps are valid"))
                    })
                    .collect()
            })
        }

        fn change_state(
            &self,
            transition: gst::StateChange,
        ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
            if transition == gst::StateChange::PausedToReady {
                self.state().reset(None);
                self.source_position_ns
                    .store(UNKNOWN_POSITION_NS, Ordering::Relaxed);
            }
            self.parent_change_state(transition)
        }
    }
}

glib::wrapper! {
    pub struct GstPhaseVocoder(ObjectSubclass<imp::GstPhaseVocoder>)
        @extends gst::Element, gst::Object;
}

impl GstPhaseVocoder {
    pub fn source_position(&self) -> Option<gst::ClockTime> {
        let position = self.imp().source_position_ns.load(Ordering::Relaxed);
        (position != UNKNOWN_POSITION_NS).then(|| gst::ClockTime::from_nseconds(position))
    }

    pub fn set_source_position(&self, position: gst::ClockTime) {
        self.imp()
            .source_position_ns
            .store(position.nseconds(), Ordering::Relaxed);
    }

    pub fn set_playback_speed(&self, playback_speed: f32) -> Result<()> {
        if !playback_speed.is_finite()
            || !(MIN_PLAYBACK_SPEED..=MAX_PLAYBACK_SPEED).contains(&playback_speed)
        {
            bail!("playback speed is outside the supported range");
        }
        self.set_property("playback-speed", playback_speed);
        Ok(())
    }

    pub fn bypass(&self) -> bool {
        self.property("bypass")
    }

    pub fn set_bypass(&self, bypass: bool) {
        self.set_property("bypass", bypass);
    }
}

pub fn register() -> Result<()> {
    gst::Element::register(
        None,
        "rustphasevocoder",
        gst::Rank::NONE,
        GstPhaseVocoder::static_type(),
    )
    .context("could not register the in-process phase-vocoder element")
}

#[cfg(test)]
mod tests {
    use gst::prelude::*;
    use gstreamer as gst;
    use gstreamer_app as gst_app;

    use super::GstPhaseVocoder;

    #[test]
    fn bypass_forwards_pcm_without_modification() {
        gst::init().unwrap();
        let caps = gst::Caps::builder("audio/x-raw")
            .field("format", "F32LE")
            .field("layout", "interleaved")
            .field("rate", 48_000_i32)
            .field("channels", 2_i32)
            .build();
        let source = gst_app::AppSrc::builder()
            .caps(&caps)
            .format(gst::Format::Time)
            .build();
        let phase_vocoder = gst::glib::Object::builder::<GstPhaseVocoder>()
            .property("playback-speed", 0.5_f32)
            .property("bypass", true)
            .build();
        let sink = gst_app::AppSink::builder().sync(false).build();
        let pipeline = gst::Pipeline::new();
        pipeline
            .add_many([
                source.upcast_ref::<gst::Element>(),
                phase_vocoder.upcast_ref::<gst::Element>(),
                sink.upcast_ref::<gst::Element>(),
            ])
            .unwrap();
        gst::Element::link_many([
            source.upcast_ref::<gst::Element>(),
            phase_vocoder.upcast_ref::<gst::Element>(),
            sink.upcast_ref::<gst::Element>(),
        ])
        .unwrap();

        let samples = [0.0_f32, 0.25, -0.5, 0.75, 1.0, -1.0, 0.125, -0.125];
        let bytes = samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect::<Vec<_>>();
        let mut input = gst::Buffer::from_slice(bytes.clone());
        if let Some(input) = input.get_mut() {
            input.set_pts(gst::ClockTime::from_seconds(2));
            input.set_duration(gst::ClockTime::from_nseconds(4 * 1_000_000_000 / 48_000));
        }

        pipeline.set_state(gst::State::Playing).unwrap();
        source.push_buffer(input).unwrap();
        source.end_of_stream().unwrap();
        let sample = sink.pull_sample().unwrap();
        let output = sample.buffer().unwrap();
        let output_map = output.map_readable().unwrap();
        assert_eq!(output_map.as_slice(), bytes);
        assert_eq!(output.pts(), Some(gst::ClockTime::from_seconds(2)));
        pipeline.set_state(gst::State::Null).unwrap();
    }

    #[test]
    fn element_negotiates_processes_and_drains_to_eos() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let source = gst::ElementFactory::make("audiotestsrc")
            .property("num-buffers", 100_i32)
            .property("samplesperbuffer", 512_i32)
            .build()
            .unwrap();
        let convert = gst::ElementFactory::make("audioconvert").build().unwrap();
        let caps_filter = gst::ElementFactory::make("capsfilter").build().unwrap();
        caps_filter.set_property(
            "caps",
            gst::Caps::builder("audio/x-raw")
                .field("format", "F32LE")
                .field("layout", "interleaved")
                .field("rate", 48_000_i32)
                .field("channels", 2_i32)
                .build(),
        );
        let phase_vocoder = gst::glib::Object::builder::<GstPhaseVocoder>()
            .property("playback-speed", 0.5_f32)
            .build();
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .build()
            .unwrap();
        let elements = [
            source,
            convert,
            caps_filter,
            phase_vocoder.upcast::<gst::Element>(),
            sink,
        ];
        pipeline.add_many(elements.iter()).unwrap();
        gst::Element::link_many(elements.iter()).unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();

        let bus = pipeline.bus().unwrap();
        let message = bus
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(10),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .expect("pipeline did not finish");
        if let gst::MessageView::Error(error) = message.view() {
            panic!("phase-vocoder pipeline failed: {}", error.error());
        }
        assert!(matches!(message.view(), gst::MessageView::Eos(_)));
        pipeline.set_state(gst::State::Null).unwrap();
    }
}
