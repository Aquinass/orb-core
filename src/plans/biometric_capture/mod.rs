//! Biometric capture.

use crate::{
    agents::{
        camera, mirror,
        python::{face_identifier, ir_net, ir_net::EstimateOutput, rgb_net},
    },
    brokers::{BrokerFlow, Orb, OrbPlan},
    config::Config,
    consts::{
        CONTINUOUS_CALIBRATION_REDUCER, IRIS_BRIGHTNESS_RANGE, IRIS_SCORE_MIN, IRIS_SHARPNESS_MIN,
        RGB_REDUCED_HEIGHT, RGB_REDUCED_WIDTH, THRESHOLD_OCCLUSION_30,
    },
    ext::broadcast::ReceiverExt as _,
    logger::{LogOnError, DATADOG, NO_TAGS},
    mcu::{self, main::IrLed},
    pid::{derivative::LowPassFilter, InstantTimer, Timer},
    port,
};
use eyre::Result;
use futures::{future::Fuse, prelude::*};
use ordered_float::OrderedFloat;
use rand::random;
use std::{
    collections::VecDeque,
    mem::take,
    pin::Pin,
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tokio::time;

/// Minimal viable sharpness.
pub const MIN_SHARPNESS: f64 = 1.2;

/// IR frame pixel mean value.
pub const IR_TARGET_MEAN: f64 = 135.0;

/// Occlusion low pass filter to act as moving average.
const OCCLUSION_CENTER_LED_LOW_PASS_FILTER_RC: f64 = 0.4;

/// Delay before the occlusion indicator can turn off after being set.
const OCCLUSION_INDICATOR_MIN_TIME_INTERVAL: Duration = Duration::from_millis(450);

/// Biometric data captured for one of the user's eyes.
#[derive(Debug, Clone, Default)]
pub struct EyeCapture {
    /// IR frame.
    pub ir_frame: camera::ir::Frame,
    /// IR frame in 940 nm.
    pub ir_frame_940nm: Option<camera::ir::Frame>,
    /// IR frame in 740 nm.
    pub ir_frame_740nm: Option<camera::ir::Frame>,
    /// IR-Net estimate.
    pub ir_net_estimate: ir_net::EstimateOutput,
    /// RGB frame.
    pub rgb_frame: camera::rgb::Frame,
    /// RGB-Net estimate.
    pub rgb_net_estimate: rgb_net::EstimateOutput,
}

#[derive(Debug, Clone, Default)]
/// Face frame and RGB-Net estimate for face self-custody candidate.
pub struct SelfCustodyCandidate {
    /// RGB frame.
    pub rgb_frame: camera::rgb::Frame,
    /// RGB-Net estimate on eyes landmarks.
    pub rgb_net_eye_landmarks: (rgb_net::Point, rgb_net::Point),
    /// RGB-Net estimate on face bounding box.
    pub rgb_net_bbox: rgb_net::Rectangle,
}

/// Combined data for both eyes.
#[derive(Clone, Debug, Default)]
pub struct Capture {
    /// Data for the left eye.
    pub eye_left: EyeCapture,
    /// Data for the right eye.
    pub eye_right: EyeCapture,
    /// Candidate data for self-custody face.
    pub face_self_custody_candidate: SelfCustodyCandidate,
    /// Average GPS latitude during capture.
    pub latitude: Option<f64>,
    /// Average GPS longitude during capture.
    pub longitude: Option<f64>,
}

/// Configuration history of the biometric capture.
#[derive(Debug)]
pub struct Log {
    /// IR eye camera configuration history.
    pub ir_eye_camera: camera::ir::Log,
    /// IR front camera configuration history.
    pub ir_face_camera: camera::ir::Log,
    /// Microcontroller configuration history.
    pub main_mcu: mcu::main::Log,
    /// Movable mirrors configuration history.
    pub mirror: mirror::Log,
}

/// Biometric capture output.
#[derive(Debug)]
pub struct Output {
    /// Biometric data if the capture was successful.
    pub capture: Option<Capture>,
    /// Configuration history.
    pub log: Log,
}

/// Biometric capture plan.
#[allow(missing_docs, clippy::struct_excessive_bools)]
pub struct Plan {
    pub objectives: VecDeque<Objective>,
    target_left_eye: bool,
    timeout: Fuse<Pin<Box<time::Sleep>>>,
    timed_out: bool,
    left_ir: Option<FrameInfoIr>,
    left_rgb: Option<FrameInfoRgb>,
    right_ir: Option<FrameInfoIr>,
    right_rgb: Option<FrameInfoRgb>,
    self_custody_candidate_rgb: Option<FrameInfoSelfCustodyCandidate>,
    latitude: Option<f64>,
    longitude: Option<f64>,
    gps_points: usize,
    max_sharpness: f64,
    total_objectives: usize,
    occlusion_center_led_timer: InstantTimer,
    occlusion_30_filter: LowPassFilter,
    occlusion_indicator_on_time: Option<Instant>,
    mirror_offsets: Vec<mirror::Point>,
}

/// Biometric capture objective.
#[allow(missing_docs)]
#[derive(Debug)]
pub struct Objective {
    pub target_left_eye: bool,
    pub ir_led_wavelength: IrLed,
    pub ir_led_duration: u16,
    pub only_rgb_net_frames: bool,
}

type FrameInfoIr = FrameInfo<ir_net::EstimateOutput, camera::ir::Frame>;
type FrameInfoRgb = FrameInfo<rgb_net::EstimateOutput, camera::rgb::Frame>;
type FrameInfoSelfCustodyCandidate =
    FrameInfo<face_identifier::types::IsValidOutput, camera::rgb::Frame>;

struct FrameInfo<T, U> {
    _timestamp: Instant,
    estimate: T,
    frame: U,
}

impl<T, U> FrameInfo<T, U> {
    fn new(estimate: T, frame: U) -> Self {
        Self { _timestamp: Instant::now(), estimate, frame }
    }
}

impl OrbPlan for Plan {
    fn handle_ir_net(
        &mut self,
        orb: &mut Orb,
        output: port::Output<ir_net::Model>,
        frame: Option<camera::ir::Frame>,
    ) -> Result<BrokerFlow> {
        match output.value {
            ir_net::Output::Estimate(estimate) => {
                self.update_occlusion(orb, &estimate);
                if let Some(perceived_side) = estimate.perceived_side {
                    if perceived_side != i32::from(!self.target_left_eye) {
                        tracing::debug!("Skipping frame due to target and perceived side mismatch");
                        return Ok(BrokerFlow::Continue);
                    }
                } else {
                    tracing::debug!("IRNet perceived_side=None, skipping frame");
                    return Ok(BrokerFlow::Continue);
                }

                self.update_ux(orb, estimate.sharpness);

                let frame = frame.expect("frame must be set for an estimate output");
                let valid_capture = estimate.score >= IRIS_SCORE_MIN
                    && (!orb.ir_auto_exposure.is_enabled()
                        || IRIS_BRIGHTNESS_RANGE.contains(&frame.mean()));

                if valid_capture {
                    let slot =
                        if self.target_left_eye { &mut self.left_ir } else { &mut self.right_ir };
                    if slot.is_none() {
                        DATADOG.incr(
                            "orb.main.count.signup.during.biometric_capture.\
                             first_side_sharp_iris_detected",
                            [format!(
                                "side:{}",
                                if self.target_left_eye { "left" } else { "right" }
                            )],
                        )?;
                    }
                    tracing::debug!("Found sharp iris: {}", estimate.score);
                    *slot = Some(FrameInfoIr::new(estimate, frame));
                }
            }
            ir_net::Output::Version(_) => {}
            ir_net::Output::Error => {
                tracing::error!("IR-Net failed during biometric capture phase");
            }
            ir_net::Output::Warmup => unreachable!("IR-Net::Warmup not part biometric capture"),
        }
        Ok(BrokerFlow::Continue)
    }

    fn handle_rgb_net(
        &mut self,
        _orb: &mut Orb,
        output: port::Output<rgb_net::Model>,
        frame: Option<camera::rgb::Frame>,
    ) -> Result<BrokerFlow> {
        if let rgb_net::Output::Estimate(estimate) = output.value {
            if let Some(prediction) = estimate.primary() {
                if prediction.bbox.coordinates.is_correct() {
                    let frame = frame.expect("frame must be set for an estimate output");
                    let slot =
                        if self.target_left_eye { &mut self.left_rgb } else { &mut self.right_rgb };
                    *slot = Some(FrameInfoRgb::new(estimate, frame));
                }
            }
        }
        Ok(BrokerFlow::Continue)
    }

    fn handle_face_identifier(
        &mut self,
        orb: &mut Orb,
        output: port::Output<face_identifier::Model>,
        frame: Option<camera::rgb::Frame>,
    ) -> Result<BrokerFlow> {
        if let face_identifier::Output::IsValidImage(output) = output.value {
            tracing::debug!("Face self-custody frame score: {:?}", output.score);
            if output.error.is_some() {
                tracing::error!("Face self-custody frame error: {:?}", output);
            }

            if output.is_valid.map_or(false, |v| v) {
                let highest = self
                    .self_custody_candidate_rgb
                    .as_ref()
                    .map_or(0.0, |p| p.estimate.score.unwrap_or_default());
                if output.score.is_some_and(|s| s > highest) {
                    tracing::info!(
                        "New face self-custody frame captured with score: {:?}",
                        output.score
                    );
                    self.self_custody_candidate_rgb = Some(FrameInfoSelfCustodyCandidate::new(
                        output,
                        frame.expect("frame must be set for FaceIdentifier::IsValidImage"),
                    ));
                }

                orb.only_rgb_net_frames = true;
            }
        }
        Ok(BrokerFlow::Continue)
    }

    fn poll_extra(&mut self, orb: &mut Orb, cx: &mut Context<'_>) -> Result<BrokerFlow> {
        while let Poll::Ready(output) = orb.main_mcu.rx_mut().next_broadcast().poll_unpin(cx) {
            if let mcu::main::Output::Gps(message) = output? {
                self.track_gps(message);
            }
        }

        let (rgb, ir) = if self.target_left_eye {
            (&self.left_rgb, &self.left_ir)
        } else {
            (&self.right_rgb, &self.right_ir)
        };

        if let (Some(_rgb), Some(_ir)) = (rgb, ir) {
            if !self.is_last_objective() {
                return Ok(BrokerFlow::Break);
            }
            if self.self_custody_candidate_rgb.is_some() {
                return Ok(BrokerFlow::Break);
            }
        }

        if let Poll::Ready(()) = self.timeout.poll_unpin(cx) {
            self.timed_out = true;
            return Ok(BrokerFlow::Break);
        }
        Ok(BrokerFlow::Continue)
    }
}

impl Plan {
    /// Creates a new biometric capture plan.
    #[must_use]
    pub fn new(wavelengths: &[(IrLed, u16)], timeout: Option<Duration>, _config: &Config) -> Self {
        let target_left_eye: bool = random();
        let mut objectives = VecDeque::new();
        for (target_left_eye, only_rgb_net_frames) in
            [(target_left_eye, true), (!target_left_eye, false)]
        {
            for &(ir_led_wavelength, ir_led_duration) in wavelengths {
                objectives.push_back(Objective {
                    target_left_eye,
                    ir_led_wavelength,
                    ir_led_duration,
                    only_rgb_net_frames,
                });
            }
        }
        let total_objectives = objectives.len();
        tracing::debug!("OBJECTIVES {:?}", objectives);
        Self {
            objectives,
            target_left_eye: false,
            timeout: timeout
                .map_or_else(Fuse::terminated, |timeout| Box::pin(time::sleep(timeout)).fuse()),
            timed_out: false,
            left_ir: None,
            left_rgb: None,
            right_ir: None,
            right_rgb: None,
            self_custody_candidate_rgb: None,
            latitude: None,
            longitude: None,
            gps_points: 0,
            max_sharpness: 0.0,
            total_objectives,
            occlusion_center_led_timer: InstantTimer::default(),
            occlusion_30_filter: LowPassFilter::default(),
            occlusion_indicator_on_time: None,
            mirror_offsets: Vec::new(),
        }
    }

    /// Runs the biometric capture plan.
    ///
    /// # Panics
    ///
    /// If `wavelength` given to the [`Plan::new`] constructor was empty.
    pub async fn run(mut self, orb: &mut Orb) -> Result<Output> {
        self.run_pre(orb).await?;
        loop {
            orb.run(&mut self).await?;
            if self.run_check(orb).await? {
                break;
            }
        }
        self.run_post(orb).await
    }

    pub(crate) async fn run_pre(&mut self, orb: &mut Orb) -> Result<()> {
        orb.main_mcu.rx_mut().clear()?;
        orb.main_mcu.log_start();
        orb.enable_ir_net().await?;
        orb.enable_rgb_net(false).await?; // Forward RGB frames to both RGB-Net and FaceIdentifier.
        orb.start_ir_eye_camera().await?;
        orb.start_ir_face_camera().await?;
        orb.start_rgb_camera().await?;
        if orb.config.lock().await.thermal_camera {
            orb.start_thermal_camera().await?;
        }
        orb.enable_mirror()?;
        orb.enable_distance()?;
        orb.start_ir_auto_focus(MIN_SHARPNESS, true).await?;
        orb.enable_eye_tracker()?;
        orb.enable_eye_pid_controller()?;
        orb.start_ir_auto_exposure(IR_TARGET_MEAN).await?;
        orb.set_fisheye(RGB_REDUCED_WIDTH, RGB_REDUCED_HEIGHT, false).await?;
        tracing::info!("Starting biometric capture with {} objectives", self.objectives.len());
        assert!(self.set_next_objective(orb).await?, "given no wavelengths");
        // Start with negative occlusion.
        self.occlusion_30_filter.reset();
        self.occlusion_30_filter.add(
            THRESHOLD_OCCLUSION_30 * 1.5,
            0.0,
            OCCLUSION_CENTER_LED_LOW_PASS_FILTER_RC,
        );
        Ok(())
    }

    pub(crate) async fn run_check(&mut self, orb: &mut Orb) -> Result<bool> {
        self.mirror_offsets.push(orb.mirror_offset.expect("already be populated"));
        if self.timed_out {
            tracing::info!("Biometric capture timeout");
            return Ok(true);
        }
        if !self.set_next_objective(orb).await? {
            DATADOG.incr(
                "orb.main.count.signup.during.biometric_capture.both_eye_captured",
                NO_TAGS,
            )?;
            tracing::info!("All objectives achieved");
            return Ok(true);
        }
        Ok(false)
    }

    pub(crate) async fn run_post(mut self, orb: &mut Orb) -> Result<Output> {
        orb.disable_ir_net();
        orb.disable_rgb_net();
        orb.disable_ir_auto_exposure();
        orb.try_enable_eye_tracker();
        orb.stop_eye_tracker().await?;
        orb.try_enable_ir_auto_focus();
        orb.stop_ir_auto_focus().await?;
        orb.stop_distance().await?;
        if orb.thermal_camera.is_enabled() {
            orb.stop_thermal_camera().await?;
        }
        orb.stop_rgb_camera().await?;
        orb.try_enable_eye_pid_controller();
        orb.stop_eye_pid_controller().await?;

        let log_ir_eye_camera = orb.stop_ir_eye_camera().await?;
        let log_ir_face_camera = orb.stop_ir_face_camera().await?;
        let log_main_mcu = orb.main_mcu.log_stop();

        let mirror_offsets = take(&mut self.mirror_offsets);
        let capture = self.into_capture();
        if capture.is_some() {
            continuous_calibration(orb, mirror_offsets).await?;
        }

        let log = Log {
            ir_eye_camera: log_ir_eye_camera,
            ir_face_camera: log_ir_face_camera,
            main_mcu: log_main_mcu,
            mirror: orb.stop_mirror().await?,
        };

        Ok(Output { capture, log })
    }

    fn into_capture(self) -> Option<Capture> {
        let FrameInfoIr { estimate: left_ir_net_estimate, frame: left_ir_frame, .. } =
            self.left_ir?;
        let FrameInfoRgb { estimate: left_rgb_net_estimate, frame: left_rgb_frame, .. } =
            self.left_rgb?;
        let FrameInfoIr { estimate: right_ir_net_estimate, frame: right_ir_frame, .. } =
            self.right_ir?;
        let FrameInfoRgb { estimate: right_rgb_net_estimate, frame: right_rgb_frame, .. } =
            self.right_rgb?;
        let FrameInfoSelfCustodyCandidate {
            estimate: face_identifier_output,
            frame: self_custody_candidate_rgb_frame,
            ..
        } = self.self_custody_candidate_rgb?;
        let eye_left = EyeCapture {
            ir_frame: left_ir_frame,
            ir_frame_940nm: None,
            ir_frame_740nm: None,
            ir_net_estimate: left_ir_net_estimate,
            rgb_frame: left_rgb_frame,
            rgb_net_estimate: left_rgb_net_estimate,
        };
        let eye_right = EyeCapture {
            ir_frame: right_ir_frame,
            ir_frame_940nm: None,
            ir_frame_740nm: None,
            ir_net_estimate: right_ir_net_estimate,
            rgb_frame: right_rgb_frame,
            rgb_net_estimate: right_rgb_net_estimate,
        };
        Some(Capture {
            eye_left,
            eye_right,
            latitude: self.latitude,
            longitude: self.longitude,
            face_self_custody_candidate: SelfCustodyCandidate {
                rgb_frame: self_custody_candidate_rgb_frame,
                rgb_net_eye_landmarks: face_identifier_output.rgb_net_eye_landmarks,
                rgb_net_bbox: face_identifier_output.rgb_net_bbox,
            },
        })
    }

    async fn set_next_objective(&mut self, orb: &mut Orb) -> Result<bool> {
        if let Some(objective) = self.objectives.pop_front() {
            tracing::info!("Biometric capture objective: {objective:?}");
            self.max_sharpness = 0.0;
            self.target_left_eye = objective.target_left_eye;
            orb.set_target_left_eye(objective.target_left_eye).await?;
            orb.set_ir_wavelength(objective.ir_led_wavelength).await?;
            orb.set_ir_duration(objective.ir_led_duration)?;
            orb.only_rgb_net_frames = objective.only_rgb_net_frames;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn is_last_objective(&self) -> bool {
        self.objectives.is_empty()
    }

    #[allow(clippy::cast_precision_loss)]
    fn update_ux(&mut self, orb: &mut Orb, sharpness: f64) {
        const MAX_PROGRESS: f64 = 0.8;
        const FACE_IDENTIFIED_PROGRESS: f64 = 0.25;
        // self.max_sharpness should be monotonic
        self.max_sharpness = sharpness.max(self.max_sharpness);
        // one self.objectives has been popped when we first update the UX, so add 1 to its length
        // to take it into account and start the progress bar at 0.0
        let curr_objective_index = self.total_objectives - self.objectives.len() - 1;
        let curr_objective_progress = (self.max_sharpness / IRIS_SCORE_MIN).min(1.0);
        // maximum measured sharpness is used as the progress bar for all the objectives
        // we keep FACE_IDENTIFIED_PROGRESS for a concurrent process that's taken into account
        // into the progress bar
        let total_objective_progress =
            (curr_objective_index as f64 + curr_objective_progress) / self.total_objectives as f64;
        let progress = (total_objective_progress * (MAX_PROGRESS - FACE_IDENTIFIED_PROGRESS))
            + self.self_custody_candidate_rgb.as_ref().map_or(0.0, |_| FACE_IDENTIFIED_PROGRESS);
        if self.objectives.len() <= self.total_objectives / 2 {
            orb.led.biometric_capture_half_objectives_completed();
        } else if self.objectives.is_empty() {
            orb.led.biometric_capture_all_objectives_completed();
        }
        orb.led.biometric_capture_progress(progress);
    }

    #[allow(clippy::cast_precision_loss)]
    fn track_gps(&mut self, message: nmea_parser::ParsedMessage) {
        let (latitude, longitude) = match message {
            nmea_parser::ParsedMessage::Gga(message) => (message.latitude, message.longitude),
            nmea_parser::ParsedMessage::Gll(message) => (message.latitude, message.longitude),
            nmea_parser::ParsedMessage::Gns(message) => (message.latitude, message.longitude),
            nmea_parser::ParsedMessage::Rmc(message) => (message.latitude, message.longitude),
            _ => (None, None),
        };
        if let (Some(latitude), Some(longitude)) = (latitude, longitude) {
            let prev_latitude = self.latitude.unwrap_or(0.0);
            let prev_longitude = self.longitude.unwrap_or(0.0);
            self.gps_points += 1;
            self.latitude =
                Some(prev_latitude + (latitude - prev_latitude) / self.gps_points as f64);
            self.longitude =
                Some(prev_longitude + (longitude - prev_longitude) / self.gps_points as f64);
        }
    }

    // TODO: include the occlusion 90 and make it request the threshold occlusion from the python directly
    fn update_occlusion(&mut self, orb: &mut Orb, estimate: &EstimateOutput) {
        let dt = self.occlusion_center_led_timer.get_dt().unwrap_or(0.0);
        let EstimateOutput { mut occlusion_30, sharpness, .. } = *estimate;
        if occlusion_30.is_nan() || sharpness.is_nan() || sharpness < IRIS_SHARPNESS_MIN {
            occlusion_30 = THRESHOLD_OCCLUSION_30 * 1.05;
        }
        let occlusion_30_low_pass =
            self.occlusion_30_filter.add(occlusion_30, dt, OCCLUSION_CENTER_LED_LOW_PASS_FILTER_RC);
        // Apply hysteresis and a minimum pulse time.
        let occlusion_detected =
            if let Some(occlusion_indicator_on_time) = self.occlusion_indicator_on_time {
                occlusion_30_low_pass < THRESHOLD_OCCLUSION_30 * 1.025
                    || occlusion_indicator_on_time.elapsed() < OCCLUSION_INDICATOR_MIN_TIME_INTERVAL
            } else {
                occlusion_30_low_pass < THRESHOLD_OCCLUSION_30 * 0.975
            };
        if occlusion_detected {
            self.occlusion_indicator_on_time.get_or_insert_with(Instant::now);
            orb.led.biometric_capture_occlusion(true);
        } else {
            orb.led.biometric_capture_occlusion(false);
            self.occlusion_indicator_on_time = None;
        }
    }
}

/// Performs light re-calibration at the end of each successful biometric
/// capture.
///
/// At the end of each successful biometric capture we do a light
/// re-calibration. We take the resulting values of the
/// [`eye_pid_controller`](crate::agents::eye_pid_controller) agent, and do a
/// slight adjustment to the PWM angle offsets, so the next time
/// [`eye_pid_controller`](crate::agents::eye_pid_controller) makes smaller
/// offsets.
///
/// # Panics
///
/// If `mirror_offsets` contains less than 2 points.
pub async fn continuous_calibration(
    orb: &mut Orb,
    mirror_offsets: Vec<mirror::Point>,
) -> Result<()> {
    tracing::info!("Mirror offsets after successful capture: {mirror_offsets:?}");
    let horizontal = mirror_offsets
        .iter()
        .map(|point| point.horizontal)
        .min_by_key(|x| OrderedFloat(x.abs()))
        .expect("to contain at least two points");
    let vertical = mirror_offsets
        .iter()
        .map(|point| point.vertical)
        .min_by_key(|x| OrderedFloat(x.abs()))
        .expect("to contain at least two points");
    DATADOG
        .gauge("orb.main.gauge.signup.pid.success", horizontal.to_string(), ["type:horizontal"])
        .or_log();
    DATADOG
        .gauge("orb.main.gauge.signup.pid.success", vertical.to_string(), ["type:vertical"])
        .or_log();
    let mut calibration = orb.calibration().clone();
    calibration.mirror.horizontal_offset += horizontal * CONTINUOUS_CALIBRATION_REDUCER;
    calibration.mirror.vertical_offset += vertical * CONTINUOUS_CALIBRATION_REDUCER;
    calibration.store().await?;
    orb.recalibrate(calibration).await?;
    Ok(())
}
