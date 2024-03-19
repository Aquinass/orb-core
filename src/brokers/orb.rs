use super::{AgentCell, BrokerFlow};
use crate::{
    agents::{
        camera, distance, eye_pid_controller, eye_tracker, image_notary, image_uploader,
        ir_auto_exposure, ir_auto_focus, mirror,
        python::{
            face_identifier, ir_net, mega_agent_one,
            mega_agent_two::{self, FusionErrors},
            rgb_net,
        },
        qr_code,
    },
    calibration::Calibration,
    config::Config,
    consts::{
        DBUS_SIGNUP_OBJECT_PATH, DBUS_WELL_KNOWN_BUS_NAME, DEFAULT_IR_LED_DURATION,
        DEFAULT_IR_LED_WAVELENGTH, GRACEFUL_SHUTDOWN_MAX_DELAY_SECONDS, IR_CAMERA_FRAME_RATE,
        IR_LED_MAX_DURATION, IR_LED_MAX_DURATION_740NM, IR_LED_MIN_DURATION,
    },
    dbus::SupervisorProxy,
    ext::mpsc::SenderExt as _,
    fisheye, led,
    logger::{DATADOG, NO_TAGS},
    mcu,
    mcu::{main::IrLed, Mcu},
    monitor,
    plans::biometric_capture::{EyeCapture, SelfCustodyCandidate},
    port, sound,
    sound::Melody,
};
use eyre::{bail, Result, WrapErr};
use futures::{channel::mpsc, prelude::*};
use nix::unistd::sync;
use orb_macros::Broker;
use orb_wld_data_id::SignupId;
use std::{
    collections::VecDeque,
    convert::Infallible,
    ops::RangeInclusive,
    process,
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tokio::{sync::Mutex, time::sleep};

// Give the IR camera enough time to fetch the last frame before external_trigger stops.
// Give it time to take 1-2 frames.
const IR_CAMERA_STOP_DELAY: Duration =
    Duration::from_millis(2 * 1000 / IR_CAMERA_FRAME_RATE as u64);

/// Abstract Orb broker plan.
#[allow(missing_docs)]
pub trait Plan {
    fn handle_ir_auto_focus(
        &mut self,
        _orb: &mut Orb,
        _output: port::Output<ir_auto_focus::Agent>,
    ) -> Result<BrokerFlow> {
        Ok(BrokerFlow::Continue)
    }

    fn handle_ir_eye_camera(
        &mut self,
        _orb: &mut Orb,
        _output: port::Output<camera::ir::Sensor>,
    ) -> Result<BrokerFlow> {
        Ok(BrokerFlow::Continue)
    }

    fn handle_ir_face_camera(
        &mut self,
        _orb: &mut Orb,
        _output: port::Output<camera::ir::Sensor>,
    ) -> Result<BrokerFlow> {
        Ok(BrokerFlow::Continue)
    }

    fn handle_rgb_camera(
        &mut self,
        _orb: &mut Orb,
        _output: port::Output<camera::rgb::Sensor>,
    ) -> Result<BrokerFlow> {
        Ok(BrokerFlow::Continue)
    }

    fn handle_thermal_camera(
        &mut self,
        _orb: &mut Orb,
        _output: port::Output<camera::thermal::Sensor>,
    ) -> Result<BrokerFlow> {
        Ok(BrokerFlow::Continue)
    }

    fn handle_ir_net(
        &mut self,
        _orb: &mut Orb,
        _output: port::Output<ir_net::Model>,
        _frame: Option<camera::ir::Frame>,
    ) -> Result<BrokerFlow> {
        Ok(BrokerFlow::Continue)
    }

    fn handle_rgb_net(
        &mut self,
        _orb: &mut Orb,
        _output: port::Output<rgb_net::Model>,
        _frame: Option<camera::rgb::Frame>,
    ) -> Result<BrokerFlow> {
        Ok(BrokerFlow::Continue)
    }

    fn handle_mega_agent_one(
        &mut self,
        _orb: &mut Orb,
        _output: port::Output<mega_agent_one::MegaAgentOne>,
    ) -> Result<BrokerFlow> {
        Ok(BrokerFlow::Continue)
    }

    fn handle_mega_agent_two(
        &mut self,
        _orb: &mut Orb,
        _output: port::Output<mega_agent_two::MegaAgentTwo>,
    ) -> Result<BrokerFlow> {
        Ok(BrokerFlow::Continue)
    }

    fn handle_face_identifier(
        &mut self,
        _orb: &mut Orb,
        _output: port::Output<face_identifier::Model>,
        _frame: Option<camera::rgb::Frame>,
    ) -> Result<BrokerFlow> {
        Ok(BrokerFlow::Continue)
    }

    fn handle_mirror(
        &mut self,
        _orb: &mut Orb,
        _output: port::Output<mirror::Actuator>,
    ) -> Result<BrokerFlow> {
        Ok(BrokerFlow::Continue)
    }

    fn handle_qr_code(
        &mut self,
        _orb: &mut Orb,
        _output: port::Output<qr_code::Agent>,
    ) -> Result<BrokerFlow> {
        Ok(BrokerFlow::Continue)
    }

    fn poll_extra(&mut self, _orb: &mut Orb, _cx: &mut Context<'_>) -> Result<BrokerFlow> {
        Ok(BrokerFlow::Continue)
    }
}

/// The main Orb broker.
#[allow(missing_docs, clippy::struct_excessive_bools)]
#[derive(Broker)]
pub struct Orb {
    #[agent(thread)]
    pub ir_eye_camera: AgentCell<camera::ir::Sensor>,
    #[agent(thread)]
    pub ir_face_camera: AgentCell<camera::ir::Sensor>,
    #[agent(task)]
    pub rgb_camera: AgentCell<camera::rgb::Sensor>,
    #[agent(async, process)]
    pub thermal_camera: AgentCell<camera::thermal::Sensor>,
    #[agent(async, process)]
    pub mega_agent_one: AgentCell<mega_agent_one::MegaAgentOne>,
    #[agent(async, process)]
    pub mega_agent_two: AgentCell<mega_agent_two::MegaAgentTwo>,
    #[agent(default, task)]
    pub ir_auto_focus: AgentCell<ir_auto_focus::Agent>,
    #[agent(default, task)]
    pub ir_auto_exposure: AgentCell<ir_auto_exposure::Agent>,
    #[agent(default, thread)]
    pub eye_tracker: AgentCell<eye_tracker::Agent>,
    #[agent(default, task)]
    pub eye_pid_controller: AgentCell<eye_pid_controller::Agent>,
    #[agent(task)]
    pub mirror: AgentCell<mirror::Actuator>,
    #[agent(task)]
    pub distance: AgentCell<distance::Agent>,
    #[agent(default, process)]
    pub qr_code: AgentCell<qr_code::Agent>,
    #[agent(default, task)]
    pub image_uploader: AgentCell<image_uploader::Agent>,
    #[agent(default, thread)]
    pub image_notary: AgentCell<image_notary::Agent>,

    pub config: Arc<Mutex<Config>>,
    pub sound: Box<dyn sound::Player>,
    pub led: Box<dyn led::Engine>,
    pub main_mcu: Box<dyn Mcu<mcu::Main>>,
    pub net_monitor: Box<dyn monitor::net::Monitor>,
    pub cpu_monitor: Box<dyn monitor::cpu::Monitor>,
    pub dbus_conn: Option<zbus::Connection>,
    pub state_rx: Option<StateRx>,
    pub focus_matrix_code: bool,
    pub ir_eye_save_fps_override: Option<f32>,
    pub ir_face_save_fps_override: Option<f32>,
    pub thermal_save_fps_override: Option<f32>,
    pub mirror_point: Option<mirror::Point>,
    pub mirror_offset: Option<mirror::Point>,
    pub trigger_shutdown_idle: bool,
    /// Used to control if RGB camera should forward frames to the RGB-Net model exclusively, so to some other models
    /// too. e.g. the Face Identifier model.
    pub only_rgb_net_frames: bool,
    ir_net_enabled: bool,
    ir_net_frames: VecDeque<(camera::ir::Frame, Instant)>,
    rgb_net_enabled: bool,
    rgb_net_frames: VecDeque<(camera::rgb::Frame, Instant)>,

    state_tx: StateTx,
    calibration: Calibration,
    target_left_eye: bool,
    ir_led_wavelength: IrLed,
    ir_led_duration: u16,
    ir_auto_focus_use_rgb_net_estimate: bool,
    rgb_camera_fake_port: Option<port::Outer<camera::rgb::Sensor>>,
}

/// [`Orb`] builder.
#[derive(Default)]
pub struct Builder {
    config: Option<Arc<Mutex<Config>>>,
    sound: Option<Box<dyn sound::Player>>,
    led: Option<Box<dyn led::Engine>>,
    main_mcu: Option<Box<dyn Mcu<mcu::Main>>>,
    net_monitor: Option<Box<dyn monitor::net::Monitor>>,
    cpu_monitor: Option<Box<dyn monitor::cpu::Monitor>>,
    enable_state_rx: bool,
    rgb_camera_fake_port: Option<port::Outer<camera::rgb::Sensor>>,
}

/// Agent state update receivers.
pub struct StateRx {
    /// IR eye camera state receiver.
    pub ir_eye_camera_state: mpsc::Receiver<camera::State>,
    /// IR face camera state receiver.
    pub ir_face_camera_state: mpsc::Receiver<camera::State>,
    /// RGB camera state receiver.
    pub rgb_camera_state: mpsc::Receiver<camera::State>,
}

#[allow(clippy::struct_field_names)]
#[derive(Default)]
struct StateTx {
    ir_eye_camera_state: Option<mpsc::Sender<camera::State>>,
    ir_face_camera_state: Option<mpsc::Sender<camera::State>>,
    rgb_camera_state: Option<mpsc::Sender<camera::State>>,
}

async fn init_dbus() -> zbus::Result<zbus::Connection> {
    Box::pin(
        zbus::ConnectionBuilder::session()?
            .name(DBUS_WELL_KNOWN_BUS_NAME)?
            .serve_at(DBUS_SIGNUP_OBJECT_PATH, crate::dbus::Signup)?
            .build(),
    )
    .await
}

impl Builder {
    /// Builds a new [`Orb`].
    pub async fn build(self) -> Result<Orb> {
        let Self {
            config,
            sound,
            led,
            main_mcu,
            net_monitor,
            cpu_monitor,
            enable_state_rx,
            rgb_camera_fake_port,
        } = self;
        let calibration = Calibration::load_or_default().await;
        let (state_tx, state_rx) = if enable_state_rx {
            let (ir_eye_camera_state_tx, ir_eye_camera_state_rx) = mpsc::channel(1);
            let (ir_face_camera_state_tx, ir_face_camera_state_rx) = mpsc::channel(1);
            let (rgb_camera_state_tx, rgb_camera_state_rx) = mpsc::channel(1);
            (
                StateTx {
                    ir_eye_camera_state: Some(ir_eye_camera_state_tx),
                    ir_face_camera_state: Some(ir_face_camera_state_tx),
                    rgb_camera_state: Some(rgb_camera_state_tx),
                },
                Some(StateRx {
                    ir_eye_camera_state: ir_eye_camera_state_rx,
                    ir_face_camera_state: ir_face_camera_state_rx,
                    rgb_camera_state: rgb_camera_state_rx,
                }),
            )
        } else {
            (StateTx::default(), None)
        };
        let dbus_conn = init_dbus()
            .await
            .map_err(|err| {
                tracing::error!(
                    "failed to initialize dbus connection, leaving disabled; error: {err}"
                );
            })
            .ok();
        let config = config.unwrap_or_default();
        let ir_eye_save_fps_override = config.lock().await.ir_eye_save_fps_override;
        let ir_face_save_fps_override = config.lock().await.ir_face_save_fps_override;
        let thermal_save_fps_override = config.lock().await.thermal_save_fps_override;
        Ok(new_orb!(
            config,
            sound: sound.unwrap_or_else(|| Box::new(sound::Fake)),
            led: led.unwrap_or_else(|| Box::new(led::Fake)),
            main_mcu: main_mcu.unwrap_or_else(|| Box::<mcu::main::Fake>::default()),
            net_monitor: net_monitor.unwrap_or_else(|| Box::new(monitor::net::Fake)),
            cpu_monitor: cpu_monitor.unwrap_or_else(|| Box::new(monitor::cpu::Fake)),
            dbus_conn,
            calibration,
            target_left_eye: false,
            focus_matrix_code: false,
            ir_eye_save_fps_override,
            ir_face_save_fps_override,
            thermal_save_fps_override,
            mirror_point: None,
            mirror_offset: None,
            trigger_shutdown_idle: false,
            only_rgb_net_frames: true,
            ir_net_enabled: false,
            ir_net_frames: VecDeque::new(),
            rgb_net_enabled: false,
            rgb_net_frames: VecDeque::new(),
            ir_led_wavelength: DEFAULT_IR_LED_WAVELENGTH,
            ir_led_duration: DEFAULT_IR_LED_DURATION,
            ir_auto_focus_use_rgb_net_estimate: true,
            state_tx,
            state_rx,
            rgb_camera_fake_port,
        ))
    }

    /// Sets the shared config.
    #[must_use]
    pub fn config(mut self, config: Arc<Mutex<Config>>) -> Self {
        self.config = Some(config);
        self
    }

    /// Sets the sound player.
    #[must_use]
    pub fn sound(mut self, sound: Box<dyn sound::Player>) -> Self {
        self.sound = Some(sound);
        self
    }

    /// Sets the LED engine.
    #[must_use]
    pub fn led(mut self, led: Box<dyn led::Engine>) -> Self {
        self.led = Some(led);
        self
    }

    /// Sets the main MCU interface.
    #[must_use]
    pub fn main_mcu(mut self, main_mcu: Box<dyn Mcu<mcu::Main>>) -> Self {
        self.main_mcu = Some(main_mcu);
        self
    }

    /// Sets the network monitor interface.
    #[must_use]
    pub fn net_monitor(mut self, net_monitor: Box<dyn monitor::net::Monitor>) -> Self {
        self.net_monitor = Some(net_monitor);
        self
    }

    /// Sets the CPU monitor interface.
    #[must_use]
    pub fn cpu_monitor(mut self, cpu_monitor: Box<dyn monitor::cpu::Monitor>) -> Self {
        self.cpu_monitor = Some(cpu_monitor);
        self
    }

    /// Sets `enable_state_rx`.
    #[must_use]
    pub fn enable_state_rx(mut self, enable_state_rx: bool) -> Self {
        self.enable_state_rx = enable_state_rx;
        self
    }

    /// Sets `rgb_camera_fake_port`.
    #[must_use]
    pub fn rgb_camera_fake_port(
        mut self,
        rgb_camera_fake_port: port::Outer<camera::rgb::Sensor>,
    ) -> Self {
        self.rgb_camera_fake_port = Some(rgb_camera_fake_port);
        self
    }
}

impl Orb {
    /// Returns a new [`Builder`].
    #[must_use]
    pub fn builder() -> Builder {
        Builder::default()
    }

    /// Gets active IR LED wavelength.
    #[must_use]
    pub fn ir_wavelength(&self) -> IrLed {
        self.ir_led_wavelength
    }

    /// Enables the IR LED with default settings.
    /// If already active settings won't be changed.
    pub async fn enable_ir_led(&mut self) -> Result<()> {
        if matches!(self.ir_led_wavelength, IrLed::None) {
            self.set_ir_wavelength(DEFAULT_IR_LED_WAVELENGTH).await?;
        }
        if matches!(self.ir_led_duration, 0) {
            self.set_ir_duration(DEFAULT_IR_LED_DURATION)?;
        }
        Ok(())
    }

    /// Disables IR LED.
    pub async fn disable_ir_led(&mut self) -> Result<()> {
        self.set_ir_wavelength(IrLed::None).await?;
        self.set_ir_duration(0)?;
        Ok(())
    }

    /// Sets active IR LED wavelength.
    pub async fn set_ir_wavelength(&mut self, ir_led_wavelength: IrLed) -> Result<()> {
        self.main_mcu.send(mcu::main::Input::IrLed(ir_led_wavelength)).await?;
        self.ir_led_wavelength = ir_led_wavelength;
        let exposure_range = self.exposure_range();
        if let Some(ir_auto_exposure) = self.ir_auto_exposure.enabled() {
            ir_auto_exposure
                .send_unjam(port::Input::new(ir_auto_exposure::Input::SetExposureRange(
                    exposure_range,
                )))
                .await?;
        }
        Ok(())
    }

    /// Sets active IR LED PWM duration.
    pub fn set_ir_duration(&mut self, ir_led_duration: u16) -> Result<()> {
        match self.ir_led_wavelength {
            IrLed::L740 => {
                self.main_mcu.send_now(mcu::main::Input::IrLedDuration740nm(ir_led_duration))?;
            }
            _ => {
                self.main_mcu.send_now(mcu::main::Input::IrLedDuration(ir_led_duration))?;
            }
        }
        self.ir_led_duration = ir_led_duration;
        Ok(())
    }

    /// Returns `true` if the Orb currently targets the left eye.
    #[must_use]
    pub fn target_left_eye(&self) -> bool {
        self.target_left_eye
    }

    /// Targets the left eye if `target_left_eye` is `true`, or targets the
    /// right eye otherwise.
    pub async fn set_target_left_eye(&mut self, target_left_eye: bool) -> Result<()> {
        self.target_left_eye = target_left_eye;
        if let Some(eye_pid_controller) = self.eye_pid_controller.enabled() {
            eye_pid_controller
                .send_unjam(port::Input::new(eye_pid_controller::Input::SwitchEye))
                .await?;
        }
        Ok(())
    }

    /// Returns a reference to the mirror calibration.
    #[must_use]
    pub fn calibration(&self) -> &Calibration {
        &self.calibration
    }

    /// Updates the mirror calibration.
    pub async fn recalibrate(&mut self, calibration: Calibration) -> Result<()> {
        self.calibration = calibration;
        self.mirror
            .enabled()
            .unwrap()
            .send_unjam(port::Input::new(mirror::Command::Recalibrate(self.calibration.clone())))
            .await
    }

    /// Starts eye IR camera.
    pub async fn start_ir_eye_camera(&mut self) -> Result<()> {
        self.main_mcu.send(mcu::main::Input::TriggeringIrEyeCamera(true)).await?;
        self.main_mcu.send(mcu::main::Input::FrameRate(IR_CAMERA_FRAME_RATE)).await?;
        self.enable_ir_eye_camera()?;
        self.enable_ir_led().await?;
        self.ir_eye_camera
            .enabled()
            .unwrap()
            .send(port::Input::new(camera::ir::Command::Start))
            .await?;
        Ok(())
    }

    /// Stops eye IR camera.
    ///
    /// # Panics
    ///
    /// If the camera agent is not enabled.
    pub async fn stop_ir_eye_camera(&mut self) -> Result<camera::ir::Log> {
        let log =
            self.ir_eye_camera.enabled().expect("ir_eye_camera is not enabled").stop().await?;
        self.disable_ir_eye_camera();
        sleep(IR_CAMERA_STOP_DELAY).await;
        if !self.ir_face_camera.is_enabled() {
            self.disable_ir_led().await?;
        }
        self.main_mcu.send(mcu::main::Input::TriggeringIrEyeCamera(false)).await?;
        Ok(log)
    }

    /// Starts face IR camera.
    pub async fn start_ir_face_camera(&mut self) -> Result<()> {
        self.main_mcu.send(mcu::main::Input::TriggeringIrFaceCamera(true)).await?;
        self.main_mcu.send(mcu::main::Input::FrameRate(IR_CAMERA_FRAME_RATE)).await?;
        self.enable_ir_face_camera()?;
        self.enable_ir_led().await?;
        self.ir_face_camera
            .enabled()
            .unwrap()
            .send(port::Input::new(camera::ir::Command::Start))
            .await?;
        Ok(())
    }

    /// Stops face IR camera.
    ///
    /// # Panics
    ///
    /// If the camera agent is not enabled.
    pub async fn stop_ir_face_camera(&mut self) -> Result<camera::ir::Log> {
        let log =
            self.ir_face_camera.enabled().expect("ir_face_camera is not enabled").stop().await?;
        self.disable_ir_face_camera();
        sleep(IR_CAMERA_STOP_DELAY).await;
        if !self.ir_eye_camera.is_enabled() {
            self.disable_ir_led().await?;
        }
        self.main_mcu.send(mcu::main::Input::TriggeringIrFaceCamera(false)).await?;
        Ok(log)
    }

    /// Resets RGB camera.
    ///
    /// This method ensures that no stale frames leak into the next capture by
    /// fully restarting the internal gstreamer pipeline.
    pub async fn reset_rgb_camera(&mut self) -> Result<()> {
        self.enable_rgb_camera()?;
        self.rgb_camera
            .enabled()
            .unwrap()
            .send(port::Input::new(camera::rgb::Command::Reset))
            .await?;
        self.disable_rgb_camera();
        Ok(())
    }

    /// Starts RGB camera.
    pub async fn start_rgb_camera(&mut self) -> Result<()> {
        self.only_rgb_net_frames = true;
        self.enable_rgb_camera()?;
        self.rgb_camera
            .enabled()
            .unwrap()
            .send(port::Input::new(camera::rgb::Command::Start))
            .await?;
        Ok(())
    }

    /// Stops RGB camera.
    ///
    /// # Panics
    ///
    /// If the camera agent is not enabled.
    pub async fn stop_rgb_camera(&mut self) -> Result<()> {
        self.rgb_camera
            .enabled()
            .expect("rgb_camera is not enabled")
            .send(port::Input::new(camera::rgb::Command::Stop))
            .await?;
        self.disable_rgb_camera();
        Ok(())
    }

    /// Starts the thermal camera
    ///
    /// # Panics
    ///
    /// If the camera agent is not enabled
    pub async fn start_thermal_camera(&mut self) -> Result<()> {
        self.enable_thermal_camera().await?;
        self.thermal_camera
            .enabled()
            .unwrap()
            .send(port::Input::new(camera::thermal::Command::Start))
            .await?;
        Ok(())
    }

    /// Stops the thermal camera
    ///
    /// # Panics
    ///
    /// If the camera agent is not enabled
    pub async fn stop_thermal_camera(&mut self) -> Result<()> {
        self.thermal_camera
            .enabled()
            .expect("thermal camera is not enabled")
            .send(port::Input::new(camera::thermal::Command::Stop))
            .await?;
        self.disable_thermal_camera();
        Ok(())
    }

    /// Starts IR auto-exposure agent.
    pub async fn start_ir_auto_exposure(&mut self, target_mean: f64) -> Result<()> {
        self.enable_ir_auto_exposure()?;
        let exposure_range = self.exposure_range();
        let ir_auto_exposure = self.ir_auto_exposure.enabled().unwrap();
        ir_auto_exposure
            .send_unjam(port::Input::new(ir_auto_exposure::Input::SetTargetMean(target_mean)))
            .await?;
        ir_auto_exposure
            .send_unjam(port::Input::new(ir_auto_exposure::Input::SetExposureRange(exposure_range)))
            .await?;
        Ok(())
    }

    /// Stops the distance agent.
    ///
    /// # Panics
    ///
    /// If the agent is not enabled.
    pub async fn stop_distance(&mut self) -> Result<()> {
        self.distance
            .enabled()
            .expect("distance is not enabled")
            .send_unjam(port::Input::new(distance::Input::Reset))
            .await?;
        self.disable_distance();
        Ok(())
    }

    /// Starts IR auto-focus agent.
    pub async fn start_ir_auto_focus(
        &mut self,
        min_sharpness: f64,
        use_rgb_estimate: bool,
    ) -> Result<()> {
        self.ir_auto_focus_use_rgb_net_estimate = use_rgb_estimate;
        self.enable_ir_auto_focus()?;
        if let Some(ir_auto_focus) = self.ir_auto_focus.enabled() {
            ir_auto_focus
                .send_unjam(port::Input::new(ir_auto_focus::Input::SetMinSharpness(min_sharpness)))
                .await?;
        }
        Ok(())
    }

    /// Initializes the image_notary agent with the given `signup_id`.
    pub async fn start_image_notary(&mut self, signup_id: SignupId, is_opt_in: bool) -> Result<()> {
        self.enable_image_notary()?;
        self.image_notary
            .enabled()
            .unwrap()
            .send(port::Input::new(image_notary::Input::InitializeSignup { signup_id, is_opt_in }))
            .await?;
        Ok(())
    }

    /// Stops the IR auto-focus agent.
    ///
    /// # Panics
    ///
    /// If the agent is not enabled.
    pub async fn stop_ir_auto_focus(&mut self) -> Result<()> {
        self.ir_auto_focus
            .enabled()
            .expect("ir_auto_focus is not enabled")
            .send_unjam(port::Input::new(ir_auto_focus::Input::Reset))
            .await?;
        self.main_mcu.send(mcu::main::Input::LiquidLens(None)).await?;
        self.disable_ir_auto_focus();
        Ok(())
    }

    /// Stops the eye tracker agent.
    ///
    /// # Panics
    ///
    /// If the agent is not enabled.
    pub async fn stop_eye_tracker(&mut self) -> Result<Option<mirror::Point>> {
        self.eye_tracker
            .enabled()
            .expect("eye_tracker is not enabled")
            .send_unjam(port::Input::new(eye_tracker::Input::Reset))
            .await?;
        self.disable_eye_tracker();
        Ok(self.mirror_point.take())
    }

    /// Stops the eye PID-controller agent.
    ///
    /// # Panics
    ///
    /// If the agent is not enabled.
    pub async fn stop_eye_pid_controller(&mut self) -> Result<Option<mirror::Point>> {
        self.eye_pid_controller
            .enabled()
            .expect("eye_pid_controller is not enabled")
            .send_unjam(port::Input::new(eye_pid_controller::Input::Reset))
            .await?;
        self.disable_eye_pid_controller();
        Ok(self.mirror_offset.take())
    }

    /// Stops the mirror agent.
    ///
    /// # Panics
    ///
    /// If the agent is not enabled.
    pub async fn stop_mirror(&mut self) -> Result<mirror::Log> {
        let mirror_log = self.mirror.enabled().expect("mirror is not enabled").take_log().await?;
        self.disable_mirror();
        Ok(mirror_log)
    }

    /// Stops the image saver agent.
    ///
    /// # Panics
    ///
    /// If the agent is not enabled.
    pub async fn stop_image_notary(
        &mut self,
        eyes: Option<(EyeCapture, EyeCapture, SelfCustodyCandidate)>,
    ) -> Result<(image_notary::Log, Option<image_notary::IdentificationImages>)> {
        let image_notary = self.image_notary.enabled().expect("image_notary is not enabled");
        image_notary.send(port::Input::new(image_notary::Input::FinalizeSignup)).await?;
        let mut identification_images = None;
        if let Some((left, right, self_custody_candidate)) = eyes {
            identification_images = image_notary
                .save_identification_images(left, right, self_custody_candidate)
                .await
                .unwrap_or_default();
        }
        let image_notary_log = image_notary.take_log().await?;
        self.disable_image_notary();
        Ok((image_notary_log, identification_images))
    }

    /// Enables IR-Net model.
    pub async fn enable_ir_net(&mut self) -> Result<()> {
        self.enable_mega_agent_one().await?;
        self.ir_net_enabled = true;
        Ok(())
    }

    /// Enables RGB-Net model.
    pub async fn enable_rgb_net(&mut self, only_rgb_net_frames: bool) -> Result<()> {
        self.enable_mega_agent_two().await?;
        self.rgb_net_enabled = true;
        self.only_rgb_net_frames = only_rgb_net_frames;
        Ok(())
    }

    /// Disables IR-Net model.
    pub fn disable_ir_net(&mut self) {
        self.ir_net_enabled = false;
    }

    /// Disables RGB-Net model.
    pub fn disable_rgb_net(&mut self) {
        self.only_rgb_net_frames = true;
        self.rgb_net_enabled = false;
    }

    /// Returns `true` if IR-Net model is enabled.
    #[must_use]
    pub fn is_ir_net_enabled(&self) -> bool {
        self.ir_net_enabled && self.mega_agent_one.is_enabled()
    }

    /// Returns `true` if RGB-Net model is enabled.
    #[must_use]
    pub fn is_rgb_net_enabled(&self) -> bool {
        self.rgb_net_enabled && self.mega_agent_two.is_enabled()
    }

    /// Sets fisheye parameters for RGB camera.
    pub async fn set_fisheye(
        &mut self,
        rgb_width: u32,
        rgb_height: u32,
        undistortion_enabled: bool,
    ) -> Result<()> {
        tracing::info!("Setting fisheye for {rgb_width}x{rgb_height}");
        let fisheye_config = fisheye::Config { rgb_width, rgb_height };
        if let Some(eye_tracker) = self.eye_tracker.enabled() {
            eye_tracker
                .send_unjam(port::Input::new(eye_tracker::Input::Fisheye(Some(fisheye_config))))
                .await?;
        }
        if let Some(rgb_camera) = self.rgb_camera.enabled() {
            rgb_camera
                .send_unjam(port::Input::new(camera::rgb::Command::Fisheye {
                    fisheye_config,
                    undistortion_enabled,
                }))
                .await?;
        }
        Ok(())
    }

    fn send_ir_net_estimate(&mut self, input: ir_net::Input) -> Result<()> {
        let frame = if let ir_net::Input::Estimate { frame, .. } = &input {
            frame.clone()
        } else {
            panic!("not an Input::Estimate");
        };
        let input = port::Input::new(mega_agent_one::Input::IRNet(input));
        let source_ts = input.source_ts;
        match self.mega_agent_one.enabled().unwrap().tx.try_send(input) {
            Ok(()) => self.ir_net_frames.push_back((frame, source_ts)),
            Err(err) if err.is_full() => {}
            Err(err) => bail!("message pass failed: {}", err),
        }
        Ok(())
    }

    fn send_rgb_net_estimate(&mut self, frame: &camera::rgb::Frame) -> Result<()> {
        let input = port::Input::new(mega_agent_two::Input::RgbNet(rgb_net::Input::Estimate {
            frame: frame.clone(),
        }));
        let source_ts = input.source_ts;
        match self.mega_agent_two.enabled().unwrap().tx.try_send(input) {
            Ok(()) => self.rgb_net_frames.push_back((frame.clone(), source_ts)),
            Err(err) if err.is_full() => {}
            Err(err) => bail!("message pass failed: {}", err),
        }
        Ok(())
    }

    fn send_rgb_net_face_identifier_input(&mut self, frame: &camera::rgb::Frame) -> Result<()> {
        let input = port::Input::new(mega_agent_two::Input::FusionRgbNetFaceIdentifier {
            frame: frame.clone(),
        });
        let source_ts = input.source_ts;
        match self.mega_agent_two.enabled().unwrap().tx.try_send(input) {
            Ok(()) => self.rgb_net_frames.push_back((frame.clone(), source_ts)),
            Err(err) if err.is_full() => {}
            Err(err) => bail!("message pass failed: {}", err),
        }
        Ok(())
    }

    async fn init_mega_agent_one(&mut self) -> Result<mega_agent_one::MegaAgentOne> {
        Ok((&*self.config.lock().await).into())
    }

    async fn init_mega_agent_two(&mut self) -> Result<mega_agent_two::MegaAgentTwo> {
        Ok((&*self.config.lock().await).into())
    }

    fn init_ir_eye_camera(&mut self) -> camera::ir::Sensor {
        camera::ir::Sensor::eye(self.state_tx.ir_eye_camera_state.take())
    }

    fn init_ir_face_camera(&mut self) -> camera::ir::Sensor {
        camera::ir::Sensor::face(self.state_tx.ir_face_camera_state.take())
    }

    fn init_rgb_camera(&mut self) -> camera::rgb::Sensor {
        camera::rgb::Sensor::new(
            self.state_tx.rgb_camera_state.take(),
            self.rgb_camera_fake_port.take(),
        )
    }

    async fn init_thermal_camera(&mut self) -> Result<camera::thermal::Sensor> {
        Ok((&*self.config.lock().await).into())
    }

    fn init_mirror(&mut self) -> mirror::Actuator {
        mirror::Actuator { calibration: self.calibration.clone() }
    }

    fn init_distance(&mut self) -> distance::Agent {
        distance::Agent { sound: self.sound.clone(), led: self.led.clone() }
    }

    fn handle_ir_eye_camera(
        &mut self,
        plan: &mut dyn Plan,
        output: port::Output<camera::ir::Sensor>,
    ) -> Result<BrokerFlow> {
        if let Some(ir_auto_exposure) = self.ir_auto_exposure.enabled() {
            ir_auto_exposure
                .send_now(output.chain(ir_auto_exposure::Input::Frame(output.value.clone())))?;
        }
        if self.is_ir_net_enabled() {
            self.send_ir_net_estimate(ir_net::Input::Estimate {
                frame: output.value.clone(),
                target_left_eye: self.target_left_eye,
                focus_matrix_code: self.focus_matrix_code,
            })?;
        } else {
            if let Some(ir_auto_focus) = self.ir_auto_focus.enabled() {
                // forward frame to IR auto focus if IR net is not enabled for internal sharpness calculation.
                ir_auto_focus
                    .send_now(output.chain(ir_auto_focus::Input::Frame(output.value.clone())))?;
            }
            if let Some(image_notary) = self.image_notary.enabled() {
                // Timestamps are generated in the image_notary history, so send there first.
                image_notary.send_now(port::Input::new(image_notary::Input::SaveIrNetEstimate(
                    image_notary::SaveIrNetEstimateInput {
                        estimate: None,
                        frame: output.value.clone(),
                        wavelength: self.ir_led_wavelength,
                        target_left_eye: self.target_left_eye,
                        fps_override: self.ir_eye_save_fps_override,
                        log_metadata_always: true,
                    },
                )))?;
            }
        }
        plan.handle_ir_eye_camera(self, output)
    }

    fn handle_ir_face_camera(
        &mut self,
        plan: &mut dyn Plan,
        output: port::Output<camera::ir::Sensor>,
    ) -> Result<BrokerFlow> {
        if let Some(image_notary) = self.image_notary.enabled() {
            image_notary.send_now(port::Input::new(image_notary::Input::SaveIrFaceData(
                image_notary::SaveIrFaceDataInput {
                    frame: output.value.clone(),
                    wavelength: self.ir_led_wavelength,
                    fps_override: self.ir_face_save_fps_override,
                    log_metadata_always: true,
                },
            )))?;
        }
        plan.handle_ir_face_camera(self, output)
    }

    fn handle_rgb_camera(
        &mut self,
        plan: &mut dyn Plan,
        output: port::Output<camera::rgb::Sensor>,
    ) -> Result<BrokerFlow> {
        if let Some(qr_code) = self.qr_code.enabled() {
            qr_code.send_now(output.chain(qr_code::Input::Frame(output.value.clone())))?;
        }
        if self.is_rgb_net_enabled() {
            if self.only_rgb_net_frames {
                self.send_rgb_net_estimate(&output.value)?;
            } else {
                self.send_rgb_net_face_identifier_input(&output.value)?;
            }
        }
        plan.handle_rgb_camera(self, output)
    }

    fn pre_handle_rgb_net_estimate<T: port::Port>(
        &mut self,
        output: &port::Output<T>,
        estimate: &rgb_net::EstimateOutput,
    ) -> Result<()> {
        if let Some(eye_tracker) = self.eye_tracker.enabled() {
            if let Some(input) = eye_tracker::Input::track(self.target_left_eye, estimate) {
                eye_tracker.send_now(output.chain(input))?;
            }
        }
        if let Some(ir_auto_focus) = self.ir_auto_focus.enabled() {
            if self.ir_auto_focus_use_rgb_net_estimate {
                ir_auto_focus.send_now(output.chain(estimate.into()))?;
            }
        }
        if let Some(distance) = self.distance.enabled() {
            distance.send_now(output.chain(distance::Input::RgbNetEstimate(estimate.clone())))?;
        }
        Ok(())
    }

    fn handle_rgb_net(
        &mut self,
        plan: &mut dyn Plan,
        output: port::Output<rgb_net::Model>,
    ) -> Result<BrokerFlow> {
        macro_rules! restore_frame {
            () => {
                loop {
                    if let Some((frame, source_ts)) = self.rgb_net_frames.pop_front() {
                        if source_ts == output.source_ts {
                            break frame;
                        }
                    } else {
                        tracing::error!("RGB-Net frame not found");
                        return Ok(BrokerFlow::Continue);
                    }
                }
            };
        }

        let frame = if let rgb_net::Output::Estimate(estimate) = &output.value {
            let frame = restore_frame!();
            self.pre_handle_rgb_net_estimate(&output, estimate)?;
            if let Some(image_notary) = self.image_notary.enabled() {
                // Timestamps are generated in the image_notary history, so send there first.
                image_notary.send_now(port::Input::new(
                    image_notary::Input::SaveRgbNetEstimate(
                        image_notary::SaveRgbNetEstimateInput {
                            estimate: estimate.clone(),
                            frame: frame.clone(),
                            log_metadata_always: true,
                            resolution_override: None,
                        },
                    ),
                ))?;
            }
            Some(frame)
        } else if let output @ rgb_net::Output::InitUndistort = &output.value {
            tracing::warn!("Unexpected output from RGB-Net: {output:#?}");
            None
        } else {
            None
        };

        plan.handle_rgb_net(self, output, frame)
    }

    fn handle_face_identifier(
        &mut self,
        plan: &mut dyn Plan,
        output: port::Output<face_identifier::Model>,
    ) -> Result<BrokerFlow> {
        if let face_identifier::Output::IsValidImage(_) = &output.value {
            unreachable!("FaceIdentifier::IsValidImage should only be used with fusions atm.")
        }
        plan.handle_face_identifier(self, output, None)
    }

    fn handle_fusion_rgb_net_face_identifier(
        &mut self,
        plan: &mut dyn Plan,
        source_ts: Instant,
        rn_output: rgb_net::EstimateOutput,
        fi_output: face_identifier::types::IsValidOutput,
    ) -> Result<BrokerFlow> {
        macro_rules! restore_frame {
            () => {
                loop {
                    if let Some((frame, source_ts)) = self.rgb_net_frames.pop_front() {
                        if source_ts == source_ts {
                            break frame;
                        }
                    } else {
                        unreachable!("Fusion RGB-Net and Face Identifier frame not found");
                    }
                }
            };
        }

        let rn_port_out =
            port::Output { value: rgb_net::Output::Estimate(rn_output.clone()), source_ts };
        let fi_port_out = port::Output {
            value: face_identifier::Output::IsValidImage(fi_output.clone()),
            source_ts,
        };

        let frame = restore_frame!();

        self.pre_handle_rgb_net_estimate(&rn_port_out, &rn_output)?;

        if let Some(image_notary) = self.image_notary.enabled() {
            // Timestamps are generated in the image_notary history, so send there first.
            image_notary.send_now(port::Input::new(image_notary::Input::SaveFusionRnFi(
                image_notary::SaveFusionRnFiInput {
                    estimate: rn_output,
                    is_valid: fi_output,
                    // TODO: Can this be optimized to use our frame buffer and avoid a serialization/deserialization?
                    frame: frame.clone(),
                    log_metadata_always: true,
                },
            )))?;
        }

        // In fusion agents we make the assumption that there is an order in decision making.
        if let BrokerFlow::Break = plan.handle_rgb_net(self, rn_port_out, Some(frame.clone()))? {
            return Ok(BrokerFlow::Break);
        }
        plan.handle_face_identifier(self, fi_port_out, Some(frame))
    }

    fn handle_thermal_camera(
        &mut self,
        plan: &mut dyn Plan,
        output: port::Output<camera::thermal::Sensor>,
    ) -> Result<BrokerFlow> {
        if let Some(image_notary) = self.image_notary.enabled() {
            image_notary.send_now(port::Input::new(image_notary::Input::SaveThermalData(
                image_notary::SaveThermalDataInput {
                    frame: output.value.clone(),
                    wavelength: self.ir_led_wavelength,
                    fps_override: self.thermal_save_fps_override,
                    log_metadata_always: true,
                },
            )))?;
        }
        plan.handle_thermal_camera(self, output)
    }

    fn handle_ir_net(
        &mut self,
        plan: &mut dyn Plan,
        output: port::Output<ir_net::Model>,
    ) -> Result<BrokerFlow> {
        macro_rules! restore_frame {
            () => {
                loop {
                    if let Some((frame, source_ts)) = self.ir_net_frames.pop_front() {
                        if source_ts == output.source_ts {
                            break frame;
                        }
                    } else {
                        tracing::error!("IR-Net frame not found");
                        return Ok(BrokerFlow::Continue);
                    }
                }
            };
        }

        let mut frame = None;
        if let ir_net::Output::Estimate(estimate) = &output.value {
            let frame = frame.insert(restore_frame!());
            if let Some(image_notary) = self.image_notary.enabled() {
                // Timestamps are generated in the image_notary history, so send there first.
                image_notary.send_now(port::Input::new(image_notary::Input::SaveIrNetEstimate(
                    image_notary::SaveIrNetEstimateInput {
                        estimate: Some(estimate.clone()),
                        frame: frame.clone(),
                        wavelength: self.ir_led_wavelength,
                        target_left_eye: self.target_left_eye,
                        fps_override: self.ir_eye_save_fps_override,
                        log_metadata_always: true,
                    },
                )))?;
            }
            if let Some(ir_auto_focus) = self.ir_auto_focus.enabled() {
                ir_auto_focus.send_now(output.chain(estimate.into()))?;
            }
            if let Some(eye_pid_controller) = self.eye_pid_controller.enabled() {
                eye_pid_controller.send_now(
                    output.chain(eye_pid_controller::Input::IrNetEstimate(estimate.clone())),
                )?;
            }
            if let Some(distance) = self.distance.enabled() {
                distance
                    .send_now(output.chain(distance::Input::IrNetEstimate(estimate.clone())))?;
            }
        }

        plan.handle_ir_net(self, output, frame)
    }

    fn handle_mega_agent_one(
        &mut self,
        plan: &mut dyn Plan,
        output: port::Output<mega_agent_one::MegaAgentOne>,
    ) -> Result<BrokerFlow> {
        let source_ts = output.source_ts;
        match output.value {
            mega_agent_one::Output::IRNet(value) => {
                self.handle_ir_net(plan, port::Output { value, source_ts })
            }
            _ => plan.handle_mega_agent_one(self, output),
        }
    }

    fn handle_mega_agent_two(
        &mut self,
        plan: &mut dyn Plan,
        output: port::Output<mega_agent_two::MegaAgentTwo>,
    ) -> Result<BrokerFlow> {
        let source_ts = output.source_ts;
        match output.value {
            mega_agent_two::Output::RgbNet(value) => {
                self.handle_rgb_net(plan, port::Output { value, source_ts })
            }
            mega_agent_two::Output::FaceIdentifier(value) => {
                self.handle_face_identifier(plan, port::Output { value, source_ts })
            }
            mega_agent_two::Output::FusionRgbNetFaceIdentifier { rgb_net, face_identifier } => self
                .handle_fusion_rgb_net_face_identifier(plan, source_ts, rgb_net, face_identifier),
            mega_agent_two::Output::FusionError(e) => match e {
                FusionErrors::RgbNetFaceIdentifier(rne, fie) => {
                    // In fusion agents we make the assumption that there is an order in decision making.
                    if let Some(value) = rne {
                        if let BrokerFlow::Break =
                            self.handle_rgb_net(plan, port::Output { value, source_ts })?
                        {
                            return Ok(BrokerFlow::Break);
                        }
                    }
                    if let Some(value) = fie {
                        if let BrokerFlow::Break =
                            self.handle_face_identifier(plan, port::Output { value, source_ts })?
                        {
                            return Ok(BrokerFlow::Break);
                        }
                    }
                    Ok(BrokerFlow::Continue)
                }
            },
            mega_agent_two::Output::Iris(_) | mega_agent_two::Output::Config(_) => {
                plan.handle_mega_agent_two(self, output)
            }
        }
    }

    #[allow(clippy::needless_pass_by_value)]
    fn handle_ir_auto_focus(
        &mut self,
        plan: &mut dyn Plan,
        output: port::Output<ir_auto_focus::Agent>,
    ) -> Result<BrokerFlow> {
        let value = output.value;
        self.main_mcu.send_now(mcu::main::Input::LiquidLens(Some(value)))?;
        plan.handle_ir_auto_focus(self, output)
    }

    #[allow(clippy::needless_pass_by_value)]
    fn handle_ir_auto_exposure(
        &mut self,
        _plan: &mut dyn Plan,
        output: port::Output<ir_auto_exposure::Agent>,
    ) -> Result<BrokerFlow> {
        let ir_auto_exposure::Output { gain, exposure } = output.value;
        if let Some(ir_eye_camera) = self.ir_eye_camera.enabled() {
            ir_eye_camera.send_now(output.chain(camera::ir::Command::SetGain(gain)))?;
            ir_eye_camera
                .send_now(output.chain(camera::ir::Command::SetExposure(exposure.into())))?;
        }
        if let Some(ir_face_camera) = self.ir_face_camera.enabled() {
            ir_face_camera.send_now(output.chain(camera::ir::Command::SetGain(gain)))?;
            ir_face_camera
                .send_now(output.chain(camera::ir::Command::SetExposure(exposure.into())))?;
        }
        self.set_ir_duration(exposure)?;
        Ok(BrokerFlow::Continue)
    }

    #[allow(clippy::needless_pass_by_value)]
    fn handle_eye_tracker(
        &mut self,
        _plan: &mut dyn Plan,
        output: port::Output<eye_tracker::Agent>,
    ) -> Result<BrokerFlow> {
        let mirror_point = output.value;
        self.mirror_point = Some(mirror_point);
        if let Some(mirror) = self.mirror.enabled() {
            mirror.send_now(output.chain(mirror::Command::SetPoint(
                mirror_point + self.mirror_offset.unwrap_or_default(),
            )))?;
        }
        Ok(BrokerFlow::Continue)
    }

    #[allow(clippy::needless_pass_by_value)]
    fn handle_eye_pid_controller(
        &mut self,
        _plan: &mut dyn Plan,
        output: port::Output<eye_pid_controller::Agent>,
    ) -> Result<BrokerFlow> {
        let mirror_offset = output.value;
        self.mirror_offset = Some(mirror_offset);
        if let Some(mirror) = self.mirror.enabled() {
            if let Some(mirror_point) = self.mirror_point {
                mirror.send_now(
                    output.chain(mirror::Command::SetPoint(mirror_point + mirror_offset)),
                )?;
            }
        }
        Ok(BrokerFlow::Continue)
    }

    #[allow(clippy::needless_pass_by_value)]
    fn handle_mirror(
        &mut self,
        plan: &mut dyn Plan,
        output: port::Output<mirror::Actuator>,
    ) -> Result<BrokerFlow> {
        let (x, y) = output.value;
        self.main_mcu.send_now(mcu::main::Input::Mirror(x, y))?;
        plan.handle_mirror(self, output)
    }

    #[allow(clippy::unused_self, clippy::needless_pass_by_value, clippy::unnecessary_wraps)]
    fn handle_distance(
        &mut self,
        _plan: &mut dyn Plan,
        output: port::Output<distance::Agent>,
    ) -> Result<BrokerFlow> {
        match output.value {}
    }

    #[allow(clippy::unused_self, clippy::needless_pass_by_value, clippy::unnecessary_wraps)]
    fn handle_qr_code(
        &mut self,
        plan: &mut dyn Plan,
        output: port::Output<qr_code::Agent>,
    ) -> Result<BrokerFlow> {
        plan.handle_qr_code(self, output)
    }

    #[allow(clippy::unused_self, clippy::needless_pass_by_value)]
    fn handle_image_uploader(
        &mut self,
        _plan: &mut dyn Plan,
        output: port::Output<image_uploader::Agent>,
    ) -> Result<BrokerFlow> {
        match output.value {}
    }

    #[cfg_attr(test, allow(unused_variables))]
    #[allow(clippy::unused_self, clippy::needless_pass_by_value, clippy::unnecessary_wraps)]
    fn handle_image_notary(
        &mut self,
        _plan: &mut dyn Plan,
        output: port::Output<image_notary::Agent>,
    ) -> Result<BrokerFlow> {
        #[cfg(not(test))]
        match output.value {}
        #[cfg(test)]
        Ok(BrokerFlow::Continue)
    }

    fn exposure_range(&self) -> RangeInclusive<u16> {
        match self.ir_led_wavelength {
            IrLed::L740 => IR_LED_MIN_DURATION..=IR_LED_MAX_DURATION_740NM,
            _ => IR_LED_MIN_DURATION..=IR_LED_MAX_DURATION,
        }
    }

    /// Shuts down the orb.
    pub async fn shutdown(&mut self) -> Result<Infallible> {
        DATADOG.incr("orb.main.count.global.shutting_down", NO_TAGS)?;
        tracing::info!("Shutting down the Orb");
        self.sound.build(sound::Type::Melody(Melody::PoweringDown))?.priority(3).push()?.await;

        // save latest config to disk
        tracing::info!("Starting to write config to disk");
        self.config.lock().await.store().await?;
        sync(); // sync filesystem
        tracing::info!("Config written to disk");

        // shutdown comes from the MCU in last resort
        self.main_mcu.send(mcu::main::Input::Shutdown(GRACEFUL_SHUTDOWN_MAX_DELAY_SECONDS)).await?;
        let connection = zbus::Connection::session()
            .await
            .wrap_err("failed establishing a `session` dbus connection")?;
        let proxy =
            SupervisorProxy::new(&connection).await.wrap_err("failed creating supervisor proxy")?;
        tracing::info!(
            "scheduling poweroff in 0ms by calling \
             org.worldcoin.OrbSupervisor1.Manager.ScheduleShutdown"
        );
        proxy
            .schedule_shutdown("poweroff", 0)
            .await
            .wrap_err("failed to schedule poweroff to supervisor proxy")?;
        process::exit(0);
    }

    fn poll_extra(
        &mut self,
        plan: &mut dyn Plan,
        cx: &mut Context<'_>,
        _fence: Instant,
    ) -> Result<Option<Poll<()>>> {
        if matches!(plan.poll_extra(self, cx)?, BrokerFlow::Break) {
            return Ok(Some(Poll::Ready(())));
        }
        Ok(Some(Poll::Pending))
    }
}
