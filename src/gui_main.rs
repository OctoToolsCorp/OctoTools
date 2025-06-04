#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release

use anyhow::Result;
use eframe::egui;
use solana_pumpfun_token::gui::modern_app::ModernApp; // Using the new ModernApp
use std::fs;
use std::path::Path;

#[tokio::main]
async fn main() -> Result<(), eframe::Error> {
    // --- Load .env file ---
    if dotenvy::dotenv().is_err() {
        // Log a warning but don't crash if .env is missing
        log::warn!(".env file not found or failed to load. Using default settings and environment variables.");
    }
    // --- End Load .env file ---

    // Setup logging (optional, but recommended)
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    log::info!("Starting Solana Pump.fun Token Manager GUI");

    // Setup eframe options
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1000.0, 700.0]) // Default window size
            .with_min_inner_size([600.0, 400.0]), // Minimum window size
        ..Default::default()
    };

    // Run the eframe application directly with ModernApp
    eframe::run_native(
        "Octo Tools", // Window title
        options,
        Box::new(move |cc| {
            Ok(Box::new(ModernApp::new(cc))) // Directly instantiate ModernApp
        }),
    )
}