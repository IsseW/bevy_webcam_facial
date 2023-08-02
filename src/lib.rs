//use bevy::prelude::*;
use bevy::{
    app::{App, Plugin, Update},
    ecs::{
        component::Component,
        entity::Entity,
        event::{Event, EventWriter},
        system::{Commands, Query, Res, Resource},
    },
    log::{debug, error, info},
    tasks::{AsyncComputeTaskPool, Task},
};

use crossbeam_channel::{bounded, Receiver, SendError, Sender};
use futures_lite::future;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

// rscam, v4l wrapper
use rscam::Camera;
use rscam::Config;
// rustface detector
use rustface::ImageData;
// image utils
use image::{DynamicImage, ImageBuffer};

// Plugin that reads webcamera, detects face calculates frame box
// and sends coordinates to Bevy as Event.
// (Coordinates 0,0 are in the center of camera frame)
pub struct WebcamFacialPlugin {
    pub config_webcam_device: String,
    pub config_webcam_width: u32,
    pub config_webcam_height: u32,
    pub config_webcam_framerate: u32,
    pub config_webcam_autostart: bool,
}
// Plugin configuration for webcam to be accesible from plugin system
#[derive(Resource)]
pub struct WebcamFacialController {
    pub sender: Sender<WebcamFacialData>,
    pub receiver: Receiver<WebcamFacialData>,
    pub control: bool,
    pub status: Arc<AtomicBool>,
    config_device: String,
    config_width: u32,
    config_height: u32,
    config_framerate: u32,
}

#[derive(Component)]
struct WebcamFacialTask(Task<bool>);

// WebcamFacialEvent event for sending WebcamFacialData to main Bevy app
#[derive(Event)]
pub struct WebcamFacialDataEvent(pub WebcamFacialData);

// Data structure to be exchanged with Bevy
#[derive(Default, Debug)]
pub struct WebcamFacialData {
    pub center_x: i32,
    pub center_y: i32,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub score: f32,
}

impl Plugin for WebcamFacialPlugin {
    fn build(&self, app: &mut App) {
        // Add thread channels
        let (task_channel_sender, task_channel_receiver) = bounded(1);
        let task_status = Arc::new(AtomicBool::new(false));
        // Store plugins settings in resource
        let plugin = WebcamFacialController {
            sender: task_channel_sender,
            receiver: task_channel_receiver,
            control: self.config_webcam_autostart.clone(),
            status: task_status,

            config_device: self.config_webcam_device.clone(),
            config_width: self.config_webcam_width.clone(),
            config_height: self.config_webcam_height.clone(),
            config_framerate: self.config_webcam_framerate.clone(),
        };

        // Insert nesecary resources, events and systems
        app.insert_resource(plugin)
            .add_event::<WebcamFacialDataEvent>()
            .add_systems(Update, webcam_facial_task_runner);
    }
}

fn webcam_facial_task_runner(
    webcam_facial: Res<WebcamFacialController>,
    mut commands: Commands,
    mut task: Query<(Entity, &mut WebcamFacialTask)>,
    mut events: EventWriter<WebcamFacialDataEvent>,
) {
    // If enabled and not running - start task
    if webcam_facial.control & !webcam_facial.status.load(Ordering::SeqCst) {
        // Get Arc clones
        let task_running = webcam_facial.status.clone();
        let sender_clone = webcam_facial.sender.clone();

        let device_path = webcam_facial.config_device.to_string();
        let width = webcam_facial.config_width;
        let height = webcam_facial.config_height;
        let framerate = webcam_facial.config_framerate;

        info!("Starting webcam capture. Launching capture and recognition task.");
        let thread_pool = AsyncComputeTaskPool::get();

        let task = thread_pool.spawn(async move {
            // Initialize webcam
            let mut camera = Camera::new(&device_path).unwrap();
            camera
                .start(&Config {
                    interval: (1, framerate),
                    resolution: (width, height),
                    format: b"YUYV",
                    ..Default::default()
                })
                .unwrap_or_else(|_error| error!("Failed to start camera device!"));
            // Initialize face detector
            let mut detector =
                match rustface::create_detector(&"assets/NN_Models/seeta.bin".to_string()) {
                    Ok(detector) => detector,
                    Err(error) => {
                        error!("Failed to create detector: {}", error.to_string());
                        std::process::exit(1)
                    }
                };

            detector.set_min_face_size(20);
            detector.set_score_thresh(2.0);
            detector.set_pyramid_scale_factor(0.8);
            detector.set_slide_window_step(4, 4);

            while task_running.load(Ordering::SeqCst) {
                // Get frame from buffer
                let buf = camera.capture().expect("Failed to get frame!");
                let rgb_frame = yuyv_to_rgb(&buf, width as usize, height as usize);
                // Create a new ImageBuffer from converting Vec<u8>
                let image_buffer: ImageBuffer<image::Rgb<u8>, Vec<u8>> =
                    ImageBuffer::from_vec(width, height, rgb_frame)
                        .expect("Failed to create ImageBuffer");
                // Convert ImageBuffer to DynamicImage
                let image: DynamicImage = DynamicImage::ImageRgb8(image_buffer);
                // Convert to grayscale image buffer
                let gray = image.to_luma8();
                // Get Image data from buffer data
                let mut grayscale_image_data = ImageData::new(&gray, width, height);
                // Detect face data
                let faces = detector.detect(&mut grayscale_image_data);

                // Initialize zero values if face not found
                let mut facial_data = WebcamFacialData::default();

                // Get face with maximum human face probability (best candidate)
                let max_face = faces.iter().max_by_key(|p| p.score() as i32);
                match max_face {
                    Some(max_face) => {
                        debug!("Max score face: {:?}", max_face);
                        // Take face rectangle coords
                        // Calculate "nose" coords relative from center of image ( image center is 0,0)
                        facial_data.x = faces[0].bbox().x() as i32;
                        facial_data.y = faces[0].bbox().y() as i32;
                        facial_data.width = faces[0].bbox().width() as i32;
                        facial_data.height = faces[0].bbox().height() as i32;
                        facial_data.score = faces[0].score() as f32;
                        // center x = (rect_w/2 + x) - (image_w/2)
                        facial_data.center_x =
                            (facial_data.width / 2 + facial_data.x) - (width / 2) as i32;
                        facial_data.center_y =
                            (facial_data.height / 2 + facial_data.y) - (height / 2) as i32;
                    }
                    None => {
                        debug!("No faces found. Using default zero values.");
                    }
                }
                // Send processed data
                match sender_clone.send(facial_data) {
                    Ok(()) => {
                        debug!("Data from task sent.")
                    }
                    Err(SendError(data)) => {
                        error!("Failed to send task data: {:?}", data);
                    }
                }
            }
            info!("Camera stopped. Task off.");
            true
        });
        commands.spawn(WebcamFacialTask(task));
        // Set flag that we started thread
        webcam_facial.status.store(true, Ordering::SeqCst);
    }
    // If not enabled and task is running set flag to stop
    if !webcam_facial.control & webcam_facial.status.load(Ordering::SeqCst) {
        webcam_facial.status.store(false, Ordering::SeqCst);
    }
    for (entity, mut task) in &mut task {
        if let Some(_status) = future::block_on(future::poll_once(&mut task.0)) {
            // Task is complete, so remove task component from entity
            commands.entity(entity).remove::<WebcamFacialTask>();
        }
    }
    while let Ok(data) = webcam_facial.receiver.try_recv() {
        debug!("Send Bevy event {:?}", data);
        events.send(WebcamFacialDataEvent(data));
    }
}

// Converter from YUYV to RBG
fn yuyv_to_rgb(yuyv_frame: &[u8], width: usize, height: usize) -> Vec<u8> {
    let mut rgb_frame = vec![0u8; width * height * 3];
    for i in (0..width * height).step_by(2) {
        let y0 = yuyv_frame[i * 2] as f32;
        let u = yuyv_frame[i * 2 + 1] as f32;
        let y1 = yuyv_frame[i * 2 + 2] as f32;
        let v = yuyv_frame[i * 2 + 3] as f32;
        // Convert YUV to RGB
        let r0 = (y0 + 1.4075 * (v - 128.0)) as u8;
        let g0 = (y0 - 0.3455 * (u - 128.0) - (0.7169 * (v - 128.0))) as u8;
        let b0 = (y0 + 1.7790 * (u - 128.0)) as u8;
        let r1 = (y1 + 1.4075 * (v - 128.0)) as u8;
        let g1 = (y1 - 0.3455 * (u - 128.0) - (0.7169 * (v - 128.0))) as u8;
        let b1 = (y1 + 1.7790 * (u - 128.0)) as u8;
        // Fill the RGB frame with the converted pixel values
        let index = i * 3;
        rgb_frame[index] = r0;
        rgb_frame[index + 1] = g0;
        rgb_frame[index + 2] = b0;
        rgb_frame[index + 3] = r1;
        rgb_frame[index + 4] = g1;
        rgb_frame[index + 5] = b1;
    }
    rgb_frame
}
