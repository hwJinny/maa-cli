mod callback;
use callback::summary;

mod external;

mod window;

#[cfg(test)]
mod window_tests;

pub mod preset;

use std::{
    path::Path,
    sync::{Arc, atomic},
};

use anyhow::{Context, Result, bail};
use clap::Args;
use image::ImageFormat;
use log::{debug, warn};
use maa_core::Assistant;
use maa_dirs::{self as dirs, Ensure, MAA_CORE_LIB};
use maa_types::InstanceOptionKey;
use signal_hook::consts::TERM_SIGNALS;

use crate::{
    config::{
        FindFile,
        asst::AsstConfig,
        task::{TaskConfig, TaskConfigTemplate},
    },
    installer,
};

const CONNECTION_PROBE_SCHEMA_VERSION: u32 = 2;
const WIN32_PROBE_CAPABILITY_VERSION: &str = "arkconsole-win32-probe-v2";

#[derive(Debug, Clone, Default)]
struct ConnectionProbeMetadata {
    window_process_id: Option<u32>,
    window_client_width: Option<u32>,
    window_client_height: Option<u32>,
    screencap_method: Option<u64>,
    mouse_method: Option<u64>,
    keyboard_method: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
struct ScreenshotQuality {
    normalized_width: u32,
    normalized_height: u32,
    non_black_ratio: f64,
    luma_variance: f64,
    black_frame: bool,
    capture_quality_ok: bool,
}

fn analyze_screenshot_quality(png: &[u8]) -> Result<ScreenshotQuality> {
    let image = image::load_from_memory_with_format(png, ImageFormat::Png)
        .context("MaaCore returned an invalid PNG screenshot")?
        .to_rgb8();
    let width = image.width();
    let height = image.height();
    let count = u64::from(width) * u64::from(height);
    if count == 0 {
        bail!("MaaCore returned an empty PNG screenshot");
    }

    let mut non_black = 0_u64;
    let mut luma_sum = 0_f64;
    let mut luma_squared_sum = 0_f64;
    for pixel in image.pixels() {
        let [red, green, blue] = pixel.0;
        if red.max(green).max(blue) > 8 {
            non_black += 1;
        }
        let luma =
            (0.299 * f64::from(red)) + (0.587 * f64::from(green)) + (0.114 * f64::from(blue));
        luma_sum += luma;
        luma_squared_sum += luma * luma;
    }
    let sample_count = count as f64;
    let non_black_ratio = non_black as f64 / sample_count;
    let mean = luma_sum / sample_count;
    let luma_variance = (luma_squared_sum / sample_count - mean * mean).max(0.0);
    let black_frame = non_black_ratio < 0.001;
    let capture_quality_ok = !black_frame && luma_variance >= 4.0;
    Ok(ScreenshotQuality {
        normalized_width: width,
        normalized_height: height,
        non_black_ratio,
        luma_variance,
        black_frame,
        capture_quality_ok,
    })
}

fn connection_probe_success_payload(
    connection: &str,
    screenshot_bytes: usize,
    quality: Option<&ScreenshotQuality>,
    metadata: &ConnectionProbeMetadata,
) -> serde_json::Value {
    serde_json::json!({
        "ok": true,
        "schema_version": CONNECTION_PROBE_SCHEMA_VERSION,
        "capability_version": WIN32_PROBE_CAPABILITY_VERSION,
        "connection": connection,
        "screenshot_bytes": screenshot_bytes,
        "window_process_id": metadata.window_process_id,
        "window_client_width": metadata.window_client_width,
        "window_client_height": metadata.window_client_height,
        "screencap_method": metadata.screencap_method,
        "mouse_method": metadata.mouse_method,
        "keyboard_method": metadata.keyboard_method,
        "normalized_screenshot_width": quality.map(|value| value.normalized_width),
        "normalized_screenshot_height": quality.map(|value| value.normalized_height),
        "non_black_ratio": quality.map(|value| value.non_black_ratio),
        "luma_variance": quality.map(|value| value.luma_variance),
        "black_frame": quality.map(|value| value.black_frame),
        "capture_quality_ok": quality.map(|value| value.capture_quality_ok),
    })
}

fn connection_probe_failure_payload(error_code: &str) -> serde_json::Value {
    let stable_code = if !error_code.is_empty()
        && error_code.len() <= 80
        && error_code
            .bytes()
            .all(|value| value.is_ascii_lowercase() || value.is_ascii_digit() || value == b'_')
    {
        error_code
    } else {
        "connection_probe_failed"
    };
    serde_json::json!({
        "ok": false,
        "schema_version": CONNECTION_PROBE_SCHEMA_VERSION,
        "capability_version": WIN32_PROBE_CAPABILITY_VERSION,
        "error_code": stable_code,
    })
}

fn connection_capabilities_payload() -> serde_json::Value {
    serde_json::json!({
        "ok": true,
        "schema_version": CONNECTION_PROBE_SCHEMA_VERSION,
        "capability_version": WIN32_PROBE_CAPABILITY_VERSION,
        "win32_supported": cfg!(windows),
        "probe_fields": [
            "raw_window_client_size",
            "method_echo",
            "capture_quality",
            "structured_error_code",
        ],
    })
}

#[cfg_attr(test, derive(Debug, PartialEq))]
#[derive(Args, Default)]
pub struct CommonArgs {
    /// ADB serial number of device or MaaTools address set in PlayCover
    ///
    /// By default, MaaCore connects to game with ADB,
    /// and this parameter is the serial number of the device
    /// (default to `emulator-5554` if not specified here and not set in config file).
    /// And if you want to use PlayCover,
    /// you need to set the connection type to PlayCover in the config file
    /// and then you can specify the address of MaaTools here.
    #[arg(short, long, verbatim_doc_comment)]
    pub addr: Option<String>,
    /// Profile (asst config file) name
    ///
    /// A profile is a config file that contains the configuration passed to MaaCore.
    /// By default, we will try to load the config file `$MAA_CONFIG_DIR/profiles/default.toml`.
    /// If the file does not exist, we will try to load the config file `$MAA_CONFIG_DIR/asst.toml`
    /// for backward compatibility, which is the old config file name.
    /// If you want to use another config file, you can specify the profile name here.
    /// The config file should be placed in the directory `$MAA_CONFIG_DIR/profiles/`.
    #[arg(short, long, verbatim_doc_comment)]
    pub profile: Option<String>,
    /// Load resources from the config directory
    ///
    /// By default, MaaCore loads resources from the resource installed with MaaCore.
    /// If you want to modify some configuration of MaaCore or you want to use your own resources,
    /// you can use this option to load resources from the `resource` directory,
    /// which is a subdirectory of the config directory.
    ///
    /// This option can also be enabled by setting the value of the key `user_resource` to true
    /// in the asst configure file `$MAA_CONFIG_DIR/asst.toml`.
    ///
    /// Note:
    /// CLI will load resources shipped with MaaCore firstly,
    /// then some client specific or platform specific when needed,
    /// lastly, it will load resources from the config directory.
    /// MaaCore will overwrite the resources loaded before,
    /// if there are some resources with the same name.
    /// Use at your own risk!
    #[arg(long, verbatim_doc_comment)]
    pub user_resource: bool,
    /// Parse the your config but do not connect to the game
    ///
    /// This option is useful when you want to check your config file.
    /// It will parse your config file and set the log level to debug.
    /// If there are some errors in your config file,
    /// it will print the error message and exit.
    #[arg(long, verbatim_doc_comment)]
    pub dry_run: bool,
    /// Do not display task summary
    ///
    /// By default, maa will display task summary after all tasks are finished.
    /// If you want to disable this behavior, you can use this option.
    #[arg(long, verbatim_doc_comment)]
    pub no_summary: bool,
    /// Do not reconnect when game loses connection to server
    ///
    /// By default, maa will automatically reconnect when the game client
    /// loses connection to the game server. Use this option to
    /// disable this behavior for this run.
    #[arg(long, verbatim_doc_comment)]
    pub no_auto_reconnect: bool,
}

#[cfg_attr(test, derive(Debug, PartialEq))]
#[derive(Args, Default)]
pub struct ConnectionTestArgs {
    /// Profile (asst config file) name
    #[arg(short, long)]
    pub profile: Option<String>,
    /// Take one fresh screenshot after connecting
    #[arg(long)]
    pub screencap: bool,
    /// Print a machine-readable JSON result
    #[arg(long)]
    pub json: bool,
    /// Report connection probe capabilities without loading MaaCore or a profile
    #[arg(long, conflicts_with_all = ["profile", "screencap"])]
    pub capabilities: bool,
}

impl CommonArgs {
    pub fn apply_to(&self, config: &mut AsstConfig) {
        if let Some(addr) = self.addr.as_ref() {
            config.connection.set_address(addr);
        }

        if self.user_resource {
            config.resource.use_user_resource();
        }
    }
}

fn find_profile(root: impl AsRef<Path>, profile: Option<&str>) -> Result<AsstConfig> {
    let root = root.as_ref();
    if let Some(profile) = profile {
        AsstConfig::find_file(join!(root, "profiles", profile))
            .context("Failed to find profile file!")
    } else if let Some(config) = AsstConfig::find_file_or_none(join!(root, "profiles", "default"))?
    {
        Ok(config)
    } else if let Some(config) = AsstConfig::find_file_or_none(join!(root, "asst"))? {
        warn!(
            "The config file `asst.toml` is deprecated, please use `profiles/default.toml` instead!"
        );
        Ok(config)
    } else {
        Ok(AsstConfig::default())
    }
}

fn ensure_connection_supported(_connection: &crate::config::asst::ConnectionConfig) -> Result<()> {
    #[cfg(not(windows))]
    if matches!(_connection.preset(), crate::config::asst::Preset::Win32) {
        bail!("Win32 connection is only supported on Windows");
    }
    Ok(())
}

fn connect_assistant(
    asst: &Assistant,
    connection: &crate::config::asst::ConnectionConfig,
    address_override: Option<&str>,
) -> Result<ConnectionProbeMetadata> {
    match connection.connection_args()? {
        crate::config::asst::ConnectionArgs::Win32(args) => {
            #[cfg(windows)]
            {
                let library = dirs::find_library();
                window::validate_win32_control_unit_at(library.as_deref())?;
                let resolved = window::resolve_window_with_metadata(&args.selector)?;
                asst.async_attach_window(
                    resolved.handle,
                    args.screencap_method,
                    args.mouse_method,
                    args.keyboard_method,
                    true,
                )?;
                Ok(ConnectionProbeMetadata {
                    window_process_id: Some(resolved.process_id),
                    window_client_width: Some(resolved.client_width),
                    window_client_height: Some(resolved.client_height),
                    screencap_method: Some(args.screencap_method),
                    mouse_method: Some(args.mouse_method),
                    keyboard_method: Some(args.keyboard_method),
                })
            }
            #[cfg(not(windows))]
            {
                window::resolve_window(&args.selector)?;
                Ok(ConnectionProbeMetadata::default())
            }
        }
        crate::config::asst::ConnectionArgs::Adb {
            adb_path,
            address,
            config,
        } => {
            let address = address_override.unwrap_or(address.as_ref());
            asst.async_connect(adb_path.as_ref(), address, config, true)?;
            Ok(ConnectionProbeMetadata::default())
        }
    }
}

fn require_screenshot_bytes(image: Option<&[u8]>) -> Result<usize> {
    let image = image.context("Connection succeeded but MaaCore returned no screenshot")?;
    if image.is_empty() {
        bail!("Connection succeeded but MaaCore returned an empty screenshot");
    }
    Ok(image.len())
}

fn connection_label(preset: crate::config::asst::Preset) -> &'static str {
    use crate::config::asst::Preset;

    match preset {
        Preset::Adb => "ADB",
        Preset::MuMuPro => "MuMuPro",
        Preset::PlayCover => "PlayCover",
        Preset::Waydroid => "Waydroid",
        Preset::Androws => "Androws",
        Preset::Win32 => "Win32",
    }
}

pub fn test_connection(args: ConnectionTestArgs) -> Result<()> {
    if args.capabilities {
        let payload = connection_capabilities_payload();
        if args.json {
            println!("{payload}");
        } else {
            println!(
                "Connection probe capability: {} (schema {})",
                WIN32_PROBE_CAPABILITY_VERSION, CONNECTION_PROBE_SCHEMA_VERSION
            );
        }
        return Ok(());
    }
    let mut error_code = "profile_load_failed";
    let result = (|| -> Result<()> {
        let asst_config = find_profile(dirs::config(), args.profile.as_deref())?;
        error_code = "connection_platform_unsupported";
        ensure_connection_supported(&asst_config.connection)?;
        error_code = "maa_core_load_failed";
        load_core().context("Failed to load MaaCore!")?;
        error_code = "maa_core_setup_failed";
        setup_core(&asst_config)?;

        error_code = "assistant_create_failed";
        let asst = Assistant::new().context("Failed to create Assistant")?;
        asst_config.instance_options.apply_to(&asst)?;
        error_code = if matches!(
            asst_config.connection.preset(),
            crate::config::asst::Preset::Win32
        ) {
            "win32_attach_failed"
        } else {
            "adb_connect_failed"
        };
        let metadata = connect_assistant(&asst, &asst_config.connection, None)?;
        let (screenshot_bytes, quality) = if args.screencap {
            error_code = "screenshot_capture_failed";
            let screenshot = asst.get_fresh_image()?;
            let screenshot_bytes = require_screenshot_bytes(screenshot.as_deref())?;
            let screenshot = screenshot.expect("validated screenshot must be present");
            let quality = analyze_screenshot_quality(&screenshot)?;
            (screenshot_bytes, Some(quality))
        } else {
            (0, None)
        };
        let connection = connection_label(asst_config.connection.preset());
        if args.json {
            println!(
                "{}",
                connection_probe_success_payload(
                    connection,
                    screenshot_bytes,
                    quality.as_ref(),
                    &metadata,
                )
            );
        } else {
            println!(
                "Connection test succeeded ({connection}, screenshot bytes: {screenshot_bytes})"
            );
        }
        Ok(())
    })();
    if let Err(error) = result {
        if args.json {
            println!("{}", connection_probe_failure_payload(error_code));
        }
        return Err(error);
    }
    Ok(())
}

fn run_core<F>(f: F, args: CommonArgs) -> Result<()>
where
    F: FnOnce(&AsstConfig) -> Result<TaskConfig>,
{
    // Auto update hot update resource
    installer::hot_update::update()?;
    installer::resource::update(true)?;

    // Load asst config
    let mut asst_config = find_profile(dirs::config(), args.profile.as_deref())?;

    args.apply_to(&mut asst_config);
    ensure_connection_supported(&asst_config.connection)?;

    let mut task_config = f(&asst_config)?;
    if matches!(
        asst_config.connection.preset(),
        crate::config::asst::Preset::Win32
    ) {
        task_config.prepare_for_win32();
    }
    if let Some(resource) = task_config.client_type.resource() {
        asst_config.resource.use_global_resource(resource);
    }

    // Load and setup MaaCore
    load_core().context("Failed to load MaaCore!")?;
    setup_core(&asst_config)?;

    // Register signal handlers
    let stop_bool = Arc::new(std::sync::atomic::AtomicBool::new(false));
    for sig in TERM_SIGNALS {
        signal_hook::flag::register_conditional_default(*sig, Arc::clone(&stop_bool))
            .context("Failed to register signal handler!")?;
        signal_hook::flag::register(*sig, Arc::clone(&stop_bool))
            .context("Failed to register signal handler!")?;
    }

    // Create and setup Assistant
    let auto_reconnect = asst_config.behavior.auto_reconnect && !args.no_auto_reconnect;
    let (maa_callback, offline_stop) = callback::MaaCallback::new(auto_reconnect);
    let asst = Assistant::new_with_callback(maa_callback)
        .context("Failed to create Assistant: resources may not be loaded")?;
    asst_config.instance_options.apply_to(&asst)?;
    debug!("Setting client type to {}", task_config.client_type);
    asst.set_instance_option(
        InstanceOptionKey::ClientType,
        task_config.client_type.to_str(),
    )
    .context("Failed to set client type")?;

    // Register tasks to Assistant and prepare summary
    let mut task_summary = (!args.no_summary).then(summary::Summary::new);
    for task in task_config.tasks {
        let task_type = task.task_type;
        let params = serde_json::to_string_pretty(&task.params)?;
        debug!(
            "Adding task [{}] with params: {params}",
            task.name_or_default(),
        );
        let id = asst
            .append_task(task_type, params.as_str())
            .with_context(|| {
                format!(
                    "Failed to add task {} with params: {params}",
                    task.name_or_default(),
                )
            })?;

        if let Some(s) = task_summary.as_mut() {
            s.insert(id, task.name, task_type);
        }
    }
    if let Some(s) = task_summary {
        summary::init(s);
    }

    if !args.dry_run {
        #[cfg(target_os = "macos")]
        let playcover_address = matches!(
            asst_config.connection.preset(),
            crate::config::asst::Preset::PlayCover
        )
        .then(|| asst_config.connection.connect_args().1.into_owned());

        // Launch external apps
        let app: Option<Box<dyn external::ExternalApp>> = match asst_config.connection.preset() {
            #[cfg(target_os = "macos")]
            crate::config::asst::Preset::PlayCover => Some(Box::new(external::PlayCoverApp::new(
                task_config.client_type,
                playcover_address
                    .as_deref()
                    .context("PlayCover address is unavailable")?,
            ))),
            #[cfg(target_os = "linux")]
            crate::config::asst::Preset::Waydroid => Some(Box::new(external::WaydroidApp::new())),
            _ => None,
        };

        // Startup external app or query its runtime address if available
        let runtime_address = app
            .as_deref()
            .map(|app| app.open(task_config.start_app))
            .transpose()?
            .flatten();

        // Connect to game or emulator
        connect_assistant(&asst, &asst_config.connection, runtime_address.as_deref())?;

        debug!("Starting MAA...");
        asst.start()?;

        while asst.running() {
            if stop_bool.load(atomic::Ordering::Relaxed) {
                bail!("Interrupted by user!");
            }
            if offline_stop.load(atomic::Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        debug!("Stopping MAA...");
        asst.stop()?;

        // Close external app
        if let (Some(app), true) = (app.as_deref(), task_config.close_app) {
            debug!("Closing external app...");
            app.close().context("Failed to close external app")?;
        }
    }

    // TODO: Better ways to restore signal handlers?
    stop_bool.store(true, atomic::Ordering::Relaxed);

    Ok(())
}

// Wrapper for run_core, always try to display summary even if error occurred
// It's safe to display summary even if summary is not initialized
pub fn run<F>(f: F, args: CommonArgs) -> Result<()>
where
    F: FnOnce(&AsstConfig) -> Result<TaskConfig>,
{
    let ret = run_core(f, args);

    summary::display();

    ret?;

    if callback::MAA_CORE_ERRORED.load(atomic::Ordering::Relaxed) {
        bail!("Some error occurred during running task!");
    }

    Ok(())
}

pub fn run_preset(params: impl preset::IntoTaskConfig, args: CommonArgs) -> Result<()> {
    run(|config| params.into_task_config(config), args)
}

pub fn run_custom(path: impl AsRef<Path>, args: CommonArgs) -> Result<()> {
    run(
        |_| {
            let path = path.as_ref();
            let config = if let Some(abs_path) = dirs::abs_config(path, Some("tasks")) {
                TaskConfigTemplate::find_file(abs_path)
            } else {
                TaskConfigTemplate::find_file(path)
            }
            .context("Failed to find task file!")?;

            config.init().context("Failed to initialize task config!")
        },
        args,
    )
}

pub fn core_version() -> Result<String> {
    load_core()?;

    let v_str = Assistant::get_version().context("Failed to get MaaCore version!")?;

    Assistant::unload()?;

    Ok(v_str)
}

fn load_core() -> Result<()> {
    if Assistant::loaded() {
        debug!("MaaCore already loaded");
        return Ok(());
    }

    if let Some(lib_dir) = dirs::find_library() {
        debug!("Loading MaaCore from: {}", lib_dir.display());
        Assistant::load(lib_dir.join(MAA_CORE_LIB))
    } else {
        debug!("MaaCore not found, trying to load from system library path");
        Assistant::load(MAA_CORE_LIB)
    }
    .context("Failed to load MaaCore!")?;

    Ok(())
}

fn setup_core(config: &AsstConfig) -> Result<()> {
    debug!("Setting user directory: {}", dirs::state().display());
    Assistant::set_user_dir(dirs::state().ensure()?).context("Failed to set user directory!")?;

    config.static_options.apply()?;
    config.resource.load()?;

    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::{
        env::{self, temp_dir},
        io::Cursor,
    };

    use image::{DynamicImage, ImageFormat, Rgb, RgbImage};

    use super::*;

    #[test]
    fn screenshot_probe_requires_a_non_empty_image() {
        assert!(require_screenshot_bytes(None).is_err());
        assert!(require_screenshot_bytes(Some(&[])).is_err());
        assert_eq!(require_screenshot_bytes(Some(&[1, 2, 3])).unwrap(), 3);
    }

    fn png_bytes(image: RgbImage) -> Vec<u8> {
        let mut output = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(image)
            .write_to(&mut output, ImageFormat::Png)
            .unwrap();
        output.into_inner()
    }

    #[test]
    fn screenshot_quality_rejects_black_or_flat_frames() {
        let black = png_bytes(RgbImage::from_pixel(32, 32, Rgb([0, 0, 0])));
        let black_quality = analyze_screenshot_quality(&black).unwrap();
        assert!(black_quality.black_frame);
        assert!(!black_quality.capture_quality_ok);
        assert_eq!(black_quality.non_black_ratio, 0.0);

        let flat = png_bytes(RgbImage::from_pixel(32, 32, Rgb([240, 240, 240])));
        let flat_quality = analyze_screenshot_quality(&flat).unwrap();
        assert!(!flat_quality.black_frame);
        assert!(!flat_quality.capture_quality_ok);
        assert_eq!(flat_quality.non_black_ratio, 1.0);
        assert!(flat_quality.luma_variance < f64::EPSILON);

        assert!(analyze_screenshot_quality(b"not-a-png").is_err());

        let varied = png_bytes(RgbImage::from_fn(32, 32, |x, y| {
            if (x + y) % 2 == 0 {
                Rgb([235, 245, 250])
            } else {
                Rgb([12, 35, 52])
            }
        }));
        let varied_quality = analyze_screenshot_quality(&varied).unwrap();
        assert!(!varied_quality.black_frame);
        assert!(varied_quality.capture_quality_ok);
        assert!(varied_quality.non_black_ratio > 0.99);
        assert!(varied_quality.luma_variance > 100.0);
    }

    #[test]
    fn win32_probe_json_contract_is_versioned_and_echoes_methods() {
        let quality = ScreenshotQuality {
            normalized_width: 1280,
            normalized_height: 720,
            non_black_ratio: 0.75,
            luma_variance: 42.5,
            black_frame: false,
            capture_quality_ok: true,
        };
        let metadata = ConnectionProbeMetadata {
            window_process_id: Some(4242),
            window_client_width: Some(1920),
            window_client_height: Some(1080),
            screencap_method: Some(2),
            mouse_method: Some(128),
            keyboard_method: Some(4),
        };
        let payload = connection_probe_success_payload("Win32", 4096, Some(&quality), &metadata);

        assert_eq!(payload["ok"], true);
        assert_eq!(payload["schema_version"], CONNECTION_PROBE_SCHEMA_VERSION);
        assert_eq!(
            payload["capability_version"],
            WIN32_PROBE_CAPABILITY_VERSION
        );
        assert_eq!(payload["window_client_width"], 1920);
        assert_eq!(payload["window_client_height"], 1080);
        assert_eq!(payload["normalized_screenshot_width"], 1280);
        assert_eq!(payload["normalized_screenshot_height"], 720);
        assert_eq!(payload["screencap_method"], 2);
        assert_eq!(payload["mouse_method"], 128);
        assert_eq!(payload["keyboard_method"], 4);
        assert_eq!(payload["black_frame"], false);
        assert_eq!(payload["capture_quality_ok"], true);
    }

    #[test]
    fn probe_failure_json_exposes_only_a_stable_error_code() {
        let payload = connection_probe_failure_payload("win32_attach_failed");
        assert_eq!(payload["ok"], false);
        assert_eq!(payload["schema_version"], CONNECTION_PROBE_SCHEMA_VERSION);
        assert_eq!(
            payload["capability_version"],
            WIN32_PROBE_CAPABILITY_VERSION
        );
        assert_eq!(payload["error_code"], "win32_attach_failed");
        assert!(payload.get("detail").is_none());
        assert!(payload.get("path").is_none());

        let sanitized = connection_probe_failure_payload("bad:C:\\Users\\name");
        assert_eq!(sanitized["error_code"], "connection_probe_failed");
    }

    #[test]
    fn connection_probe_reports_the_configured_preset() {
        use crate::config::asst::Preset;

        assert_eq!(connection_label(Preset::Adb), "ADB");
        assert_eq!(connection_label(Preset::MuMuPro), "MuMuPro");
        assert_eq!(connection_label(Preset::PlayCover), "PlayCover");
        assert_eq!(connection_label(Preset::Waydroid), "Waydroid");
        assert_eq!(connection_label(Preset::Androws), "Androws");
        assert_eq!(connection_label(Preset::Win32), "Win32");
    }

    #[test]
    fn capabilities_are_reported_without_loading_a_profile() {
        let payload = connection_capabilities_payload();
        assert_eq!(payload["ok"], true);
        assert_eq!(payload["schema_version"], 2);
        assert_eq!(
            payload["capability_version"],
            WIN32_PROBE_CAPABILITY_VERSION
        );
        assert_eq!(payload["win32_supported"], cfg!(windows));
        assert_eq!(payload["probe_fields"].as_array().unwrap().len(), 4);
    }

    #[cfg(not(windows))]
    #[test]
    fn win32_is_rejected_before_loading_core_on_non_windows() {
        let config: crate::config::asst::ConnectionConfig = toml::from_str(
            r#"
                preset = "Win32"
                window_title = "Arknights"
            "#,
        )
        .unwrap();

        assert_eq!(
            ensure_connection_supported(&config)
                .unwrap_err()
                .to_string(),
            "Win32 connection is only supported on Windows"
        );
    }

    #[test]
    #[ignore = "need installed MaaCore"]
    fn basic_ffi() {
        if env::var_os("SKIP_CORE_TEST").is_some() {
            return;
        }
        core_version().unwrap();

        assert!(!Assistant::loaded());
        load_core().unwrap();
        assert!(Assistant::loaded());
        load_core().unwrap();
        assert!(Assistant::loaded());
        Assistant::unload().unwrap();
        assert!(!Assistant::loaded());
    }

    #[test]
    fn test_find_profile() {
        let test_dir = temp_dir().join("maa_test_find_profile");
        test_dir.ensure_clean().unwrap();

        let sample_str = r#"
            [connection]
            address = "test_addr"
        "#;

        let sample_config = {
            let mut config = AsstConfig::default();
            config.connection.set_address("test_addr");
            config
        };

        assert_eq!(
            find_profile(&test_dir, None).unwrap(),
            AsstConfig::default()
        );

        let backcompat_path = test_dir.join("asst.toml");
        let default_path = test_dir.join("profiles").join("default.toml");
        let test_path = test_dir.join("profiles").join("test.toml");

        std::fs::write(&backcompat_path, sample_str).unwrap();
        assert_eq!(find_profile(&test_dir, None).unwrap(), sample_config);
        std::fs::remove_file(&backcompat_path).unwrap();

        std::fs::create_dir(test_dir.join("profiles")).unwrap();

        std::fs::write(&default_path, sample_str).unwrap();
        assert_eq!(find_profile(&test_dir, None).unwrap(), sample_config);
        std::fs::remove_file(&default_path).unwrap();

        std::fs::write(&test_path, sample_str).unwrap();
        assert_eq!(
            find_profile(&test_dir, None).unwrap(),
            AsstConfig::default()
        );
        assert_eq!(
            find_profile(&test_dir, Some("test")).unwrap(),
            sample_config
        );
        std::fs::remove_file(&test_path).unwrap();

        std::fs::remove_dir_all(&test_dir).unwrap();
    }
}
