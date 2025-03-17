// Copyright 2024 Red Hat Inc
//
// SPDX-License-Identifier: Apache-2.0 or BSD-3-Clause

#![deny(
    clippy::undocumented_unsafe_blocks,
    /* groups */
    clippy::correctness,
    clippy::suspicious,
    clippy::complexity,
    clippy::perf,
    clippy::style,
    clippy::nursery,
    //* restriction */
    clippy::dbg_macro,
    clippy::rc_buffer,
    clippy::as_underscore,
    clippy::assertions_on_result_states,
    //* pedantic */
    clippy::cast_lossless,
    clippy::cast_possible_wrap,
    clippy::ptr_as_ptr,
    clippy::bool_to_int_with_if,
    clippy::borrow_as_ptr,
    clippy::case_sensitive_file_extension_comparisons,
    clippy::cast_lossless,
    clippy::cast_ptr_alignment,
    clippy::naive_bytecount
)]
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::significant_drop_in_scrutinee,
    clippy::significant_drop_tightening
)]

use clap::ValueEnum;
use derive_more::{AsMut, AsRef};
use log::info;
use std::fmt::{Display, Formatter};
use std::path::Path;
use thiserror::Error as ThisError;
use vhost::vhost_user::{GpuBackend, VhostUserProtocolFeatures};
use vm_memory::{GuestMemoryAtomic, GuestMemoryMmap};
use vhost_user::{VhostUserDaemon, VhostUserDeviceImplementer};
use virtio_gpu::device::GpuDevice;
use virtio_gpu::{device, GpuConfig};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum GpuMode {
    #[value(name = "virglrenderer", alias("virgl-renderer"))]
    VirglRenderer,
    #[cfg(feature = "gfxstream")]
    Gfxstream,
}

impl Into<virtio_gpu::GpuMode> for GpuMode {
    fn into(self) -> virtio_gpu::GpuMode {
        match self {
            Self::VirglRenderer => {
                virtio_gpu::GpuMode::VirglRenderer
            }
            #[cfg(feature = "gfxstream")]
            GpuMode::Gfxstream => {
                virtio_gpu::GpuMode::Gfxstream
            }
        }
    }
}

impl Display for GpuMode {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::VirglRenderer => write!(f, "virglrenderer"),
            #[cfg(feature = "gfxstream")]
            Self::Gfxstream => write!(f, "gfxstream"),
        }
    }
}

#[derive(Debug, ThisError)]
pub enum StartError {
    #[error("Could not create backend: {0}")]
    CouldNotCreateBackend(device::Error),
    #[error("Could not create daemon: {0}")]
    CouldNotCreateDaemon(vhost_user::Error),
    #[error("Fatal error: {0}")]
    ServeFailed(vhost_user::Error),
}

#[derive(AsRef, AsMut)]
struct VhostUserGpuBackend(GpuDevice);

impl VhostUserDeviceImplementer for VhostUserGpuBackend {
    type Device = GpuDevice;
    type Bitmap = ();

    fn protocol_features(&self) -> VhostUserProtocolFeatures {
        VhostUserProtocolFeatures::MQ
            | VhostUserProtocolFeatures::CONFIG
            | VhostUserProtocolFeatures::REPLY_ACK
            | VhostUserProtocolFeatures::BACKEND_SEND_FD
    }

    fn set_gpu_socket(&mut self, gpu_backend: GpuBackend) {
        self.as_mut().set_gpu_socket(gpu_backend);
    }
}

pub fn start_backend(socket_path: &Path, config: GpuConfig) -> Result<(), StartError> {
    info!("Starting backend");
    let device = GpuDevice::new(config).map_err(StartError::CouldNotCreateBackend)?;
    let backend = VhostUserGpuBackend(device);

    let mut daemon = VhostUserDaemon::new(
        "vhost-gpu-backend".to_string(),
        backend,
        GuestMemoryAtomic::new(GuestMemoryMmap::new()),
    )
        .map_err(StartError::CouldNotCreateDaemon)?;

    daemon.serve(socket_path).map_err(StartError::ServeFailed)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    #[cfg(feature = "gfxstream")]
    use assert_matches::assert_matches;
    use clap::ValueEnum;

    use super::*;

    #[test]
    fn test_gpu_config_create_default_virglrenderer() {
        let config = GpuConfig::new(GpuMode::VirglRenderer, None, GpuFlags::new_default()).unwrap();
        assert_eq!(config.gpu_mode(), GpuMode::VirglRenderer);
        assert_eq!(config.capsets(), GpuConfig::DEFAULT_VIRGLRENDER_CAPSET_MASK);
    }

    #[test]
    #[cfg(feature = "gfxstream")]
    fn test_gpu_config_create_default_gfxstream() {
        let config = GpuConfig::new(GpuMode::Gfxstream, None, GpuFlags::default()).unwrap();
        assert_eq!(config.gpu_mode(), GpuMode::Gfxstream);
        assert_eq!(config.capsets(), GpuConfig::DEFAULT_GFXSTREAM_CAPSET_MASK);
    }

    #[cfg(feature = "gfxstream")]
    fn assert_invalid_gpu_config(mode: GpuMode, capset: GpuCapset, expected_capset: GpuCapset) {
        let result = GpuConfig::new(mode, Some(capset), GpuFlags::new_default());
        assert_matches!(
            result,
            Err(GpuConfigError::CapsetUnsuportedByMode(
                requested_mode,
                unsupported_capset
            )) if unsupported_capset == expected_capset && requested_mode == mode
        );
    }

    #[test]
    fn test_gpu_config_valid_combination() {
        let config = GpuConfig::new(
            GpuMode::VirglRenderer,
            Some(GpuCapset::VIRGL2),
            GpuFlags::default(),
        )
            .unwrap();
        assert_eq!(config.gpu_mode(), GpuMode::VirglRenderer);
    }

    #[test]
    #[cfg(feature = "gfxstream")]
    fn test_gpu_config_invalid_combinations() {
        assert_invalid_gpu_config(
            GpuMode::VirglRenderer,
            GpuCapset::VIRGL2 | GpuCapset::GFXSTREAM_VULKAN,
            GpuCapset::GFXSTREAM_VULKAN,
        );

        assert_invalid_gpu_config(
            GpuMode::Gfxstream,
            GpuCapset::VIRGL2 | GpuCapset::GFXSTREAM_VULKAN,
            GpuCapset::VIRGL2,
        );
    }

    #[test]
    #[cfg(feature = "gfxstream")]
    fn test_gles_required_by_gfxstream() {
        let capset = GpuCapset::GFXSTREAM_VULKAN | GpuCapset::GFXSTREAM_GLES;
        let flags = GpuFlags {
            use_gles: false,
            ..GpuFlags::new_default()
        };
        let result = GpuConfig::new(GpuMode::Gfxstream, Some(capset), flags);
        assert_matches!(result, Err(GpuConfigError::GlesRequiredByGfxstream));
    }

    #[test]
    fn test_default_num_capsets() {
        assert_eq!(GpuConfig::DEFAULT_VIRGLRENDER_CAPSET_MASK.num_capsets(), 2);
        #[cfg(feature = "gfxstream")]
        assert_eq!(GpuConfig::DEFAULT_GFXSTREAM_CAPSET_MASK.num_capsets(), 2);
    }

    #[test]
    fn test_capset_display_multiple() {
        let capset = GpuCapset::VIRGL | GpuCapset::VIRGL2;
        let output = capset.to_string();
        assert_eq!(output, "virgl, virgl2")
    }

    /// Check if display name of GpuMode is the same as the name in the CLI arg
    #[test]
    fn test_gpu_mode_display_eq_arg_name() {
        for mode in GpuMode::value_variants() {
            let mode_str = mode.to_string();
            let mode_from_str = GpuMode::from_str(&mode_str, false);
            assert_eq!(*mode, mode_from_str.unwrap());
        }
    }

    #[test]
    fn test_fail_listener() {
        // This will fail the listeners and thread will panic.
        let socket_name = Path::new("/proc/-1/nonexistent");
        let config = GpuConfig::new(GpuMode::VirglRenderer, None, GpuFlags::default()).unwrap();

        assert_matches!(
            start_backend(socket_name, config).unwrap_err(),
            StartError::ServeFailed(_)
        );
    }
}
