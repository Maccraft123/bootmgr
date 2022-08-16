#![feature(ptr_metadata)]
use std::io::{Write, stdout};
use nix::sys::reboot::{
    reboot,
    RebootMode,
};
use crossterm::{
    ExecutableCommand,
    QueueableCommand,
    cursor,
    terminal,
    style,
    event,
    event::{
        Event,
        KeyCode,
    }
};

use efivar::efi::{VariableName, VariableFlags};
use uefi::proto::device_path::DevicePath;
use regex::Regex;

use anyhow::Result;
use anyhow::Context;

use ez_input::RinputerHandle;
use ez_input::EzEvent;
use std::sync::mpsc::Sender;
use std::sync::mpsc::channel;

fn char16_to_string(buf: &[u8]) -> (String, usize) {
    let mut iter = buf.iter();
    let mut out: Vec<u16> = Vec::new();
    let mut i: usize = 0;

    loop {
        i += 2;
        if let (Some(lower), Some(upper)) = (iter.next(), iter.next()) {
            let tmp = (*upper as u16) << 8 | *lower as u16;
            if tmp == '\0' as u16 {
                break;
            } else {
                out.push(tmp);
            }
        } else {
            break;
        }
    }

    (std::char::decode_utf16(out)
        .map(|r| r.unwrap_or(' '))
        .map(|r| if r.is_ascii() {r} else {' '})
        .collect::<String>(), i)
}

#[derive(Clone, Debug, PartialEq)]
struct Entry {
    id: u16,
    id_string: String,
    description: String,
    path: Vec<String>,
    display_default: bool,
}

impl Entry {
    fn new(var: &str, buf: &[u8]) -> Self {
        let (description, end) = char16_to_string(&buf[(32+16)/8..]);
        let desc_end_offset = (32+16)/8 + end;
        let device_path: &DevicePath = unsafe {
            std::mem::transmute(&buf[desc_end_offset..])
        };

        let mut display_default = false;
        let mut out_path: Vec<String> = Vec::new();
        for node in device_path.node_iter() {
            if let Some(file) = node.as_file_path_media_device_path() {
                let path = file.path_name().to_cstring16().unwrap();
                let lowercase = path.to_string().to_lowercase();
                
                // ignore default selections
                if lowercase.contains(r"\efi\boot\bootx64.efi") ||
                    lowercase.contains(r"\efi\boot\bootia.efi") ||
                    lowercase.contains(r"\efi\boot\bootaa64.efi") {
                    display_default = false;
                } else {
                    display_default = true;
                }

                out_path.push(lowercase);
            } else {
                out_path.push(format!("{:?}", node.device_type()));
            }
        }

        let boot_id = var.to_string().split_off(4);
        let id = if let Ok(tmp) = u16::from_str_radix(&boot_id, 16) {
            tmp
        } else {
            0
        };

        Entry {
            id,
            id_string: boot_id,
            description,
            path: out_path,
            display_default,
        }
    }
}

impl std::fmt::Display for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let mut path = String::new();
        for tmp in &self.path {
            path.push_str(&tmp);
            path.push_str(" ");
        }
        path.pop(); // remove trailing space
        write!(f, "{}, at: '{}'", self.description, path)
    }
}

#[derive(Clone, Debug, PartialEq)]
enum MenuChoice {
    Entry(Entry),
    Menu,
}

#[derive(PartialEq)]
enum MenuType {
    Default,
    Advanced,
}

fn kbd_input(tx: Sender<EzEvent>) {
    loop {
        match event::read().unwrap() {
            Event::Key(key) => {
                match key.code {
                    KeyCode::Enter  => tx.send(EzEvent::South(true)).unwrap(),
                    KeyCode::Down   => tx.send(EzEvent::DirectionDown).unwrap(),
                    KeyCode::Up     => tx.send(EzEvent::DirectionUp).unwrap(),
                    _ => (),
                }
            },
            _ => (),
        }
    }
}

fn pad_input(tx: Sender<EzEvent>) {
    let mut handle = RinputerHandle::open().unwrap();
    loop {
        let ev = handle.get_event_blocking().unwrap();
        tx.send(ev).unwrap();
    }
}

fn menu(choices: &Vec<Entry>) -> Result<MenuChoice> {
    let mut out = stdout();

    let (tx1, rx) = channel::<EzEvent>();
    let tx2 = tx1.clone();
    std::thread::spawn(move || kbd_input(tx1));
    std::thread::spawn(move || pad_input(tx2));

    // prepare for neat display
    out.execute(terminal::EnterAlternateScreen)?;
    out.execute(cursor::Hide)?;
    terminal::enable_raw_mode()?;

    let mut chosen = false;
    let mut pos: usize = 0;
    let mut menu_items: Vec<MenuChoice> = Vec::new();
    let mut all_items: Vec<MenuChoice> = Vec::new();
    let mut cur_choice: Option<MenuChoice> = None;
    let mut what_to_display = MenuType::Default;

    for entry in choices.iter() {
        if entry.display_default {
            if cur_choice.is_none() {
                cur_choice = Some(MenuChoice::Entry(entry.clone()));
            }
            menu_items.push(MenuChoice::Entry(entry.clone()));
        }

        all_items.push(MenuChoice::Entry(entry.clone()));
    }

    menu_items.push(MenuChoice::Menu);

    while !chosen {
        let cur_menu = match what_to_display {
            MenuType::Default => &menu_items,
            MenuType::Advanced => &all_items,
        };
        out.queue(terminal::Clear(terminal::ClearType::All))?;
        out.queue(cursor::MoveTo(1, 1))?;
        out.queue(style::Print("Choose boot selection"))?;

        // print out default-visible options + boot menu
        out.queue(cursor::MoveTo(4, 3))?;

        for c in cur_menu.iter() {
            match c {
                MenuChoice::Entry(entry) => {
                    if what_to_display == MenuType::Default {
                        out.queue(style::Print(&entry.description))?;
                    } else {
                        out.queue(style::Print(entry.to_string()))?;
                    }
                    out.queue(cursor::MoveToNextLine(1))?;
                    out.queue(cursor::MoveToColumn(4))?;
                },
                MenuChoice::Menu => {
                    out.queue(style::Print("Advanced Boot Menu"))?;
                }
            }
        }

        out.queue(cursor::MoveTo(1, (3+pos) as u16))?;
        out.queue(style::Print("=>"))?;
        out.flush()?;

        match rx.recv()? {
            EzEvent::DirectionDown => {
                if let Some(new) = cur_menu.get(pos+1) {
                    pos += 1;
                    cur_choice = Some(new.clone());
                }
            },
            EzEvent::South(val) => {
                println!("val {}", val);
                if val == true {
                    if cur_choice != Some(MenuChoice::Menu) {
                        chosen = true;
                    } else {
                        what_to_display = MenuType::Advanced;
                    }
                }
            },
            EzEvent::DirectionUp => {
                // avoid overflow panics
                if pos > 0 {
                    if let Some(new) = cur_menu.get(pos-1) {
                        pos -= 1;
                        cur_choice = Some(new.clone());
                    }
                }
            },
            _ => {},
        }
    }

    out.execute(cursor::Show)?;
    out.execute(terminal::LeaveAlternateScreen)?;

    terminal::disable_raw_mode()?;
    Ok(cur_choice.unwrap())
}

fn main() -> Result<()> {
    let boot_xxxx = Regex::new(r"^Boot\d\d\d\d$")?;
    let mut manager = efivar::system();
    let mut buf: [u8; 1024] = [0u8; 1024];
    let mut options: Vec<Entry> = Vec::new();

    for var in manager.get_var_names().expect("Failed to get efivar names") {
        if boot_xxxx.is_match(var.variable()) {
            match manager.read(&var, &mut buf)  {
                Ok(..) => {
                    let tmp = Entry::new(var.variable(), &buf);
                    options.push(tmp);
                }
                Err(e) => eprintln!("{}", e),
            }
        }
    }

    let choice = menu(&options).context("Wrong selection chosen")?;
    if std::env::args().nth(1).unwrap_or("asdfasdf".to_string()) == "actually-boot" {
        if let MenuChoice::Entry(e) = choice {
            let next = VariableName::new("BootNext");
            let attr = VariableFlags::NON_VOLATILE | VariableFlags::BOOTSERVICE_ACCESS | VariableFlags::RUNTIME_ACCESS;
            let val: [u8; 2] = e.id.to_le_bytes();

            manager.write(&next, attr, &val).expect("Failed to write BootNext");

            reboot(RebootMode::RB_AUTOBOOT)?; // TODO: kexec into linux kernels
        } else {
            unreachable!();
        }
    } else {
        println!("{:#?}", choice);
        println!("NOT rebooting into it. to boot pass actually-boot as 1st argument");
    }

    Ok(())
}
