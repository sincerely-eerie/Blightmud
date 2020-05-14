use libtelnet_rs::{events::TelnetEvents, telnet::op_option as opt};
use log::{debug, error, info};
use signal_hook;
use std::io::{Read, Write};
use std::sync::{
    atomic::Ordering,
    mpsc::{channel, Receiver, Sender},
};
use std::thread;

mod ansi;
mod command;
mod event;
mod lua;
mod output_buffer;
mod screen;
mod session;
mod telnet;
mod timer;

use crate::command::spawn_input_thread;
use crate::event::Event;
use crate::screen::Screen;
use crate::session::{Session, SessionBuilder};
use crate::telnet::TelnetHandler;
use crate::timer::{spawn_timer_thread, TimerEvent};
use dirs;

type TelnetData = Option<Vec<u8>>;

fn spawn_receive_thread(mut session: Session) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut read_stream = if let Ok(stream) = &session.stream.lock() {
            stream.as_ref().unwrap().try_clone().unwrap()
        } else {
            error!("Failed to spawn receive stream without a live connection");
            panic!("Failed to spawn receive stream");
        };
        let writer = &session.main_writer;

        debug!("Receive stream spawned");
        loop {
            let mut data = vec![0; 1024];
            if let Ok(bytes_read) = read_stream.read(&mut data) {
                if bytes_read > 0 {
                    writer
                        .send(Event::ServerOutput(Vec::from(data.split_at(bytes_read).0)))
                        .unwrap();
                } else {
                    session.send_event(Event::Error("Connection closed".to_string()));
                    session.send_event(Event::Disconnect);
                    break;
                }
            } else {
                session.send_event(Event::Error("Connection failed".to_string()));
                session.send_event(Event::Disconnect);
                break;
            }
        }
        debug!("Receive stream closing");
    })
}

fn spawn_transmit_thread(
    mut session: Session,
    transmit_read: Receiver<Option<Vec<u8>>>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut write_stream = if let Ok(stream) = &session.stream.lock() {
            stream.as_ref().unwrap().try_clone().unwrap()
        } else {
            error!("Failed to spawn transmit stream without a live connection");
            panic!("Failed to spawn transmit stream");
        };
        let transmit_read = transmit_read;
        debug!("Transmit stream spawned");
        while let Ok(Some(data)) = transmit_read.recv() {
            if let Err(info) = write_stream.write_all(data.as_slice()) {
                session.disconnect();
                let error = format!("Failed to write to socket: {}", info).to_string();
                session.send_event(Event::Error(error));
                session.send_event(Event::Disconnect);
            }
        }
        debug!("Transmit stream closing");
    })
}

fn register_terminal_resize_listener(session: Session) -> thread::JoinHandle<()> {
    let signals = signal_hook::iterator::Signals::new(&[signal_hook::SIGWINCH]).unwrap();
    let main_thread_writer = session.main_writer;
    thread::spawn(move || {
        for _ in signals.forever() {
            main_thread_writer.send(Event::Redraw).unwrap();
        }
    })
}

fn start_logging() {
    if let Some(data_dir) = dirs::data_dir() {
        let logpath = data_dir.join("blightmud/logs");
        std::fs::create_dir_all(&logpath).unwrap();
        let logfile = logpath.join("log.txt");
        simple_logging::log_to_file(logfile.to_str().unwrap(), log::LevelFilter::Debug).unwrap();
    }
}

fn main() {
    start_logging();
    info!("Starting application");

    let (main_writer, main_thread_read): (Sender<Event>, Receiver<Event>) = channel();
    let timer_writer = spawn_timer_thread(main_writer.clone());

    let session = SessionBuilder::new()
        .main_writer(main_writer)
        .timer_writer(timer_writer)
        .build();

    let _input_thread = spawn_input_thread(session.clone());
    let _signal_thread = register_terminal_resize_listener(session.clone());

    if let Err(error) = run(main_thread_read, session) {
        println!("[!!] Panic: {}", error.to_string());
    }

    info!("Shutting down");
}

fn run(
    // TODO: This function is complex. Perhaps reduce it with some type of event router?
    main_thread_read: Receiver<Event>,
    mut session: Session,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut screen = Screen::new();
    screen.setup();

    let mut transmit_writer: Option<Sender<TelnetData>> = None;
    let mut telnet_handler = TelnetHandler::new(session.clone());

    loop {
        if session.terminate.load(Ordering::Relaxed) {
            break;
        }
        if let Ok(event) = main_thread_read.recv() {
            match event {
                Event::Prompt => {
                    let output_buffer = session.output_buffer.lock().unwrap();
                    if let Ok(script) = session.lua_script.lock() {
                        script.check_for_prompt_trigger_match(&output_buffer.prompt);
                    }
                    screen.print_prompt(&output_buffer.prompt);
                }
                Event::ServerSend(data) => {
                    if let Some(transmit_writer) = &transmit_writer {
                        transmit_writer.send(Some(data))?;
                    } else {
                        screen.print_error("No active session");
                    }
                }
                Event::ServerOutput(data) => {
                    telnet_handler.parse(&data);
                }
                Event::ServerInput(msg, check_alias) => {
                    if let Ok(script) = session.lua_script.lock() {
                        if !check_alias || !script.check_for_alias_match(&msg) {
                            screen.print_send(&msg);
                            if let Ok(mut parser) = session.telnet_parser.lock() {
                                if let TelnetEvents::DataSend(buffer) = parser.send_text(&msg) {
                                    session.main_writer.send(Event::ServerSend(buffer))?;
                                }
                            }
                        }
                    }
                }
                Event::MudOutput(msg) => {
                    if let Ok(script) = session.lua_script.lock() {
                        if !script.check_for_trigger_match(&msg) {
                            screen.print_output(&msg);
                        }
                    }
                }
                Event::Output(msg) => {
                    screen.print_output(&msg);
                }
                Event::UserInputBuffer(input_buffer) => {
                    let mut prompt_input = session.prompt_input.lock().unwrap();
                    *prompt_input = input_buffer;
                    screen.print_prompt_input(&prompt_input);
                }
                Event::Connect(host, port) => {
                    session.disconnect();
                    if session.connect(&host, port) {
                        let (writer, reader): (Sender<TelnetData>, Receiver<TelnetData>) =
                            channel();
                        spawn_receive_thread(session.clone());
                        spawn_transmit_thread(session.clone(), reader);
                        transmit_writer.replace(writer);
                    } else {
                        session.main_writer.send(Event::Error(
                            format!("Failed to connect to {}:{}", host, port).to_string(),
                        ))?;
                    }
                }
                Event::Connected => {
                    debug!("Connected to {}:{}", session.host, session.port);
                    session.lua_script.lock().unwrap().on_connect();
                }
                Event::ProtoEnabled(proto) => {
                    if let opt::GMCP = proto {
                        let mut parser = session.telnet_parser.lock().unwrap();
                        if let Some(event) = parser.subnegotiation_text(
                            opt::GMCP,
                            "Core.Hello {\"Client\":\"Blightmud\",\"Version\":\"0.1.0\"}",
                        ) {
                            if let TelnetEvents::DataSend(data) = event {
                                debug!("Sending GMCP Core.Hello");
                                session.main_writer.send(Event::ServerSend(data)).unwrap();
                                session.lua_script.lock().unwrap().on_gmcp_ready();
                            }
                        } else {
                            error!("Failed to send GMCP Core.Hello");
                        }
                    }
                }
                Event::GMCPRegister(msg) => {
                    let mut parser = session.telnet_parser.lock().unwrap();
                    if let Some(TelnetEvents::DataSend(data)) = parser.subnegotiation_text(
                        opt::GMCP,
                        &format!("Core.Supports.Add [\"{} 1\"]", msg),
                    ) {
                        session.main_writer.send(Event::ServerSend(data))?;
                    }
                }
                Event::GMCPReceive(msg) => {
                    let mut script = session.lua_script.lock().unwrap();
                    script.receive_gmcp(&msg);
                }
                Event::ScrollUp => screen.scroll_up(),
                Event::ScrollDown => screen.scroll_down(),
                Event::ScrollBottom => screen.reset_scroll(),
                Event::Error(msg) => {
                    screen.print_error(&msg);
                }
                Event::Info(msg) => {
                    screen.print_info(&msg);
                }
                Event::LoadScript(path) => {
                    info!("Loading script: {}", path);
                    let mut lua = session.lua_script.lock().unwrap();
                    if let Err(err) = lua.load_script(&path) {
                        screen.print_error(&format!("Failed to load file: {}", err));
                    } else {
                        screen.print_info(&format!("Loaded script: {}", path));
                        if session.connected.load(Ordering::Relaxed) {
                            lua.on_connect();
                            lua.on_gmcp_ready();
                        }
                    }
                }
                Event::ResetScript => {
                    info!("Clearing scripts");
                    screen.print_info("Clearing scripts...");
                    if let Ok(mut script) = session.lua_script.lock() {
                        script.reset();
                        screen.print_info("Done");
                    }
                }
                Event::AddTimedEvent(duration, count, id) => {
                    session
                        .timer_writer
                        .send(TimerEvent::Create(duration, count, id))?;
                }
                Event::TimedEvent(id) => {
                    session.lua_script.lock().unwrap().run_timed_function(id);
                }
                Event::DropTimedEvent(id) => {
                    session.lua_script.lock().unwrap().remove_timed_function(id);
                }
                Event::Redraw => {
                    screen.setup();
                    screen.reset_scroll();
                }
                Event::Disconnect => {
                    session.disconnect();
                    screen.print_info(&format!(
                        "Disconnecting from: {}:{}",
                        session.host, session.port
                    ));
                    if let Some(transmit_writer) = &transmit_writer {
                        transmit_writer.send(None)?;
                    }
                    transmit_writer = None;
                    session.send_event(Event::UserInputBuffer(String::new()));
                }
                Event::Quit => {
                    session.terminate.store(true, Ordering::Relaxed);
                    session.disconnect();
                    break;
                }
            };
            screen.flush();
        }
    }
    screen.reset();
    session.close()?;
    Ok(())
}
