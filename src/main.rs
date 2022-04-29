use std::num::NonZeroU16;

use clap::Parser;

use futures_util::StreamExt;
use retina::client::*;
use tracing_error::ErrorLayer;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, EnvFilter};

use gst::prelude::*;

use color_eyre::{eyre::bail, Result};

#[derive(Debug, Parser)]
struct Args {
    /// `rtsp://` URL to connect to.
    #[clap(long, env, parse(try_from_str))]
    url: url::Url,

    /// Username to send if the server requires authentication.
    #[clap(long, env)]
    username: Option<String>,

    /// Password; requires username.
    #[clap(long, env, requires = "username")]
    password: Option<String>,

    /// Filter to log
    #[clap(long, env = "RUST_LOG")]
    log: EnvFilter,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Parse Args
    let args = {
        #[cfg(feature = "dotenv")]
        dotenv().ok();
        Args::parse()
    };

    // Initialize
    {
        let fmt_layer = fmt::layer().with_target(false);

        tracing_subscriber::registry()
            .with(args.log)
            .with(fmt_layer)
            .with(ErrorLayer::default())
            .init();

        color_eyre::install()?;

        tracing_gst::integrate_events();
        gst::debug_remove_default_log_function();
        gst::init()?;
        gst::debug_set_default_threshold(gst::DebugLevel::Warning);
        tracing_gst::integrate_spans();
    }

    let mut session = retina::client::Session::describe(
        args.url.clone(),
        retina::client::SessionOptions::default()
            .creds(creds(args.username, args.password))
            .user_agent("Retina sdp example".to_owned()),
    )
    .await?;

    tracing::info!("SDP:\n{}\n\n", std::str::from_utf8(session.sdp())?);

    // Make audio and video streams
    {
        // Make video stream
        let video_stream_i = session.streams().iter().position(|s| {
            if s.media == "video" && s.encoding_name == "h264" {
                tracing::info!("Using {} video stream", &s.encoding_name);
                return true;
            }

            false
        });

        if let Some(i) = video_stream_i {
            session.setup(i, SetupOptions::default()).await?;
        }

        // Make audio stream
        // let audio_stream_i = session.streams().iter().position(|s| {
        //     if s.media == "audio" {
        //         tracing::info!("Using {} video stream", &s.encoding_name);
        //         return true;
        //     }

        //     false
        // });

        // if let Some(i) = audio_stream_i {
        //     session.setup(i, SetupOptions::default()).await?;
        // }

        // if video_stream_i.is_none() && audio_stream_i.is_none() {
        //     bail!("Exiting because no video or audio stream was selected; see info log messages above");
        // }
    }

    let pipeline = gst::Pipeline::new(None);

    let appsrc = {
        let appsrc = gst::ElementFactory::make("appsrc", Some("rtssrc"))?;

        {
            let appsrc = appsrc.clone().dynamic_cast::<gst_app::AppSrc>().unwrap();

            appsrc.set_stream_type(gst_app::AppStreamType::Stream);
            appsrc.set_is_live(true);
            appsrc.set_format(gst::Format::Time);
            appsrc.set_do_timestamp(true);

            appsrc.set_caps(Some(&gst::Caps::builder("application/x-rtp").build()));
        }

        appsrc
    };

    let rtpptdemux = {
        let rtpptdemux = gst::ElementFactory::make("rtpptdemux", Some("rtpptdemux"))?;

        let pipeline_weak = pipeline.downgrade();
        rtpptdemux.connect("new-payload-type", false, move |args| {
            let pt = args[1].get::<u32>().unwrap();
            let pad = args[2].get::<gst::Pad>().unwrap();

            pad.set_offset(1000000000);

            let caps = pad.caps().unwrap();
            tracing::info!("rtpptdemux: new pt={}, caps={:?}", pt, caps);

            let s = caps.structure(0).unwrap();

            let encoding_name = s.get::<&str>("encoding-name").unwrap();
            tracing::info!("encoding-name: {:?}", encoding_name);

            let launch = match encoding_name {
                "H264" => {
                    "rtph264depay \
                    ! h264parse update-timecode=true \
                    ! vaapidecodebin \
                    ! videoconvert \
                    ! autovideosink"
                }
                _ => "fakesink",
            };

            if let Some(pipeline) = pipeline_weak.upgrade() {
                let bin = gst::parse_bin_from_description(launch, true).unwrap();

                pipeline.add(&bin).unwrap();

                let sink = bin.static_pad("sink").unwrap();
                pad.link(&sink).unwrap();

                bin.set_state(gst::State::Playing).unwrap();
            }

            None
        });

        rtpptdemux
    };

    {
        pipeline.add_many(&[&appsrc, &rtpptdemux])?;
        gst::Element::link_many(&[&appsrc, &rtpptdemux])?;
    }

    pipeline.set_state(gst::State::Playing)?;

    let appsrc = appsrc.clone().dynamic_cast::<gst_app::AppSrc>().unwrap();

    let mut session = session.play(retina::client::PlayOptions::default()).await?;
    let mut bus_stream = pipeline.bus().unwrap().stream();

    loop {
        tokio::select! {
            pkt = session.next() => {
                match pkt {
                    Some(Ok(retina::client::PacketItem::RtpPacket(rtp))) => {
                        let raw = rtp.raw();

                        let stream = &session.streams()[rtp.stream_id()];

                        let mut buffer = gst::Buffer::with_size(raw.len())?;

                        {
                            let buffer = buffer.get_mut().unwrap();

                            buffer.copy_from_slice(0, raw).unwrap();
                        }

                        {
                            let clock_rate = rtp.timestamp().clock_rate().get() as i32;

                            let caps = gst::Caps::builder("application/x-rtp")
                                .field("clock-rate", clock_rate)
                                .field("payload", stream.rtp_payload_type as i32)
                                .field("media", &stream.media)
                                .field("encoding-name", &stream.encoding_name.to_uppercase());

                            let caps = if let Some(channels) = stream.channels.map(NonZeroU16::get) {
                                caps.field("channels", channels as i32)
                            } else {
                                caps
                            };

                            let caps = caps.build();

                            appsrc.set_caps(Some(&caps));
                            // application/x-rtp, payload=(int)96, media=(string)video, clock-rate=(int)90000, encoding-name=(string)H264
                        }

                        appsrc.push_buffer(buffer)?;
                    }
                    Some(Err(err)) => return Err(err.into()),
                    Some(Ok(retina::client::PacketItem::SenderReport(_sr))) => {}
                    None => {
                        let _ = appsrc.end_of_stream()?;
                        break;
                    }
                    Some(Ok(_)) => unreachable!(),
                }
            }
            msg = bus_stream.next() => {
                if let Some(msg) = msg {
                    use gst::MessageView;

                    match msg.view() {
                        MessageView::Eos(_) => break,
                        MessageView::Error(err) => bail!(err.error()),
                        _ => {},
                    }
                } else {
                    break
                }
            }
        }
    }

    pipeline.set_state(gst::State::Null)?;

    Ok(())
}

/// Interpets the `username` and `password` of a [Source].
fn creds(
    username: Option<String>,
    password: Option<String>,
) -> Option<retina::client::Credentials> {
    match (username, password) {
        (Some(username), password) => Some(retina::client::Credentials {
            username,
            password: password.unwrap_or_default(),
        }),
        (None, None) => None,
        _ => unreachable!(), // structopt/clap enforce that password requires username.
    }
}
