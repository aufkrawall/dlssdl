//! dlssdl — small egui (D3D12) tool to check NVIDIA NGX update servers for
//! DLSS SR / RR / FG and Streamline packages and download them with proper
//! consumer file names.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([840.0, 680.0])
            .with_min_inner_size([680.0, 540.0]),
        renderer: eframe::Renderer::Wgpu,
        wgpu_options: eframe::egui_wgpu::WgpuConfiguration {
            wgpu_setup: eframe::egui_wgpu::WgpuSetup::CreateNew(
                eframe::egui_wgpu::WgpuSetupCreateNew {
                    instance_descriptor: eframe::wgpu::InstanceDescriptor {
                        // Force the D3D12 renderer.
                        backends: eframe::wgpu::Backends::DX12,
                        flags: eframe::wgpu::InstanceFlags::from_build_config().with_env(),
                        backend_options: eframe::wgpu::BackendOptions::from_env_or_default(),
                        memory_budget_thresholds: Default::default(),
                        display: None,
                    },
                    ..eframe::egui_wgpu::WgpuSetupCreateNew::without_display_handle()
                },
            ),
            ..Default::default()
        },
        ..Default::default()
    };

    eframe::run_native(
        "NGX DLSS Fetcher",
        options,
        Box::new(|cc| Ok(Box::new(dlssdl::gui::DlssApp::new(cc)))),
    )
}
