use core::{convert::TryInto, marker::PhantomData, time::Duration};
use ctaphid_dispatch::app::{self as hid, Command as HidCommand, Message};
use ctaphid_dispatch::command::VendorCommand;
use apdu_dispatch::{Command as ApduCommand, command, response, app as apdu};
use apdu_dispatch::iso7816::Status;
use trussed::{
    types::Vec,
    syscall,
    Client as TrussedClient,
};

pub const USER_PRESENCE_TIMEOUT_SECS: u32 = 15;

// New commands are only available over this vendor command (acting as a namespace for this
// application).  The actual application command is stored in the first byte of the packet data.
const ADMIN: VendorCommand = VendorCommand::H72;

// For compatibility, old commands are also available directly as separate vendor commands.
const UPDATE: VendorCommand = VendorCommand::H51;
const REBOOT: VendorCommand = VendorCommand::H53;
const RNG: VendorCommand = VendorCommand::H60;
const VERSION: VendorCommand = VendorCommand::H61;
const UUID: VendorCommand = VendorCommand::H62;
const LOCKED: VendorCommand = VendorCommand::H63;

// We also handle the standard wink command.
const WINK: HidCommand = HidCommand::Wink;  // 0x08

const RNG_DATA_LEN: usize = 57;

#[derive(PartialEq)]
enum Command {
    Update,
    Reboot,
    Rng,
    Version,
    Uuid,
    Locked,
    Wink,
}

impl TryFrom<u8> for Command {
    type Error = Error;

    fn try_from(command: u8) -> Result<Self, Self::Error> {
        // First, check the old commands.
        if let Ok(command) = HidCommand::try_from(command) {
            if let Ok(command) = command.try_into() {
                return Ok(command);
            }
        }

        // Now check the new commands (none yet).
        Err(Error::UnsupportedCommand)
    }
}

impl TryFrom<HidCommand> for Command {
    type Error = Error;

    fn try_from(command: HidCommand) -> Result<Self, Self::Error> {
        match command {
            WINK => Ok(Command::Wink),
            HidCommand::Vendor(command) => command.try_into(),
            _ => Err(Error::UnsupportedCommand)
        }
    }
}

impl TryFrom<VendorCommand> for Command {
    type Error = Error;

    fn try_from(command: VendorCommand) -> Result<Self, Self::Error> {
        match command {
            UPDATE => Ok(Command::Update),
            REBOOT => Ok(Command::Reboot),
            RNG => Ok(Command::Rng),
            VERSION => Ok(Command::Version),
            UUID => Ok(Command::Uuid),
            LOCKED => Ok(Command::Locked),
            _ => Err(Error::UnsupportedCommand),
        }
    }
}

enum Error {
    InvalidLength,
    NotAvailable,
    UnsupportedCommand,
}

impl From<Error> for hid::Error {
    fn from(error: Error) -> Self {
        match error {
            Error::InvalidLength => Self::InvalidLength,
            // TODO: use more appropriate error code
            Error::NotAvailable => Self::InvalidLength,
            Error::UnsupportedCommand => Self::InvalidCommand,
        }
    }
}

impl From<Error> for Status {
    fn from(error: Error) -> Self {
        match error {
            Error::InvalidLength => Self::WrongLength,
            Error::NotAvailable => Self::ConditionsOfUseNotSatisfied,
            Error::UnsupportedCommand => Self::InstructionNotSupportedOrInvalid,
        }
    }
}

pub trait Reboot {
    /// Reboots the device.
    fn reboot() -> !;

    /// Reboots the device.
    ///
    /// Presuming the device has a separate mode of operation that
    /// allows updating its firmware (for instance, a bootloader),
    /// reboots the device into this mode.
    fn reboot_to_firmware_update() -> !;

    /// Reboots the device.
    ///
    /// Presuming the device has a separate destructive but more
    /// reliable way of rebooting into the firmware mode of operation,
    /// does so.
    fn reboot_to_firmware_update_destructive() -> !;

    /// Is device bootloader locked down?
    /// E.g., is secure boot enabled?
    fn locked() -> bool;
}

pub struct App<T, R>
where T: TrussedClient,
      R: Reboot,
{
    trussed: T,
    uuid: [u8; 16],
    version: u32,
    boot_interface: PhantomData<R>,
}

impl<T, R> App<T, R>
where T: TrussedClient,
      R: Reboot,
{
    pub fn new(client: T, uuid: [u8; 16], version: u32) -> Self {
        Self { trussed: client, uuid, version, boot_interface: PhantomData }
    }

    fn user_present(&mut self) -> bool {
        let user_present = syscall!(self.trussed.confirm_user_present(USER_PRESENCE_TIMEOUT_SECS * 1000)).result;
        user_present.is_ok()
    }

    fn exec<const N: usize>(&mut self, command: Command, flag: Option<u8>, response: &mut Vec<u8, N>) -> Result<(), Error> {
        match command {
            Command::Reboot => R::reboot(),
            Command::Locked => {
                response.push(R::locked().into()).ok();
            }
            Command::Rng => {
                // Fill the HID packet (57 bytes)
                response.extend_from_slice(
                    &syscall!(self.trussed.random_bytes(RNG_DATA_LEN)).bytes,
                ).ok();
            }
            Command::Update => {
                if self.user_present() {
                    if flag == Some(0x01) {
                        R::reboot_to_firmware_update_destructive();
                    } else {
                        R::reboot_to_firmware_update();
                    }
                } else {
                    return Err(Error::NotAvailable);
                }
            }
            Command::Uuid => {
                // Get UUID
                response.extend_from_slice(&self.uuid).ok();
            }
            Command::Version => {
                // GET VERSION
                response.extend_from_slice(&self.version.to_be_bytes()).ok();
            }
            Command::Wink => {
                debug_now!("winking");
                syscall!(self.trussed.wink(Duration::from_secs(10)));
            }
        }
        Ok(())
    }
}

impl<T, R> hid::App for App<T, R>
where T: TrussedClient,
      R: Reboot
{
    fn commands(&self) -> &'static [HidCommand] {
        &[
            HidCommand::Wink,
            HidCommand::Vendor(ADMIN),
            HidCommand::Vendor(UPDATE),
            HidCommand::Vendor(REBOOT),
            HidCommand::Vendor(RNG),
            HidCommand::Vendor(VERSION),
            HidCommand::Vendor(UUID),
            HidCommand::Vendor(LOCKED),
        ]
    }

    fn call(&mut self, command: HidCommand, input_data: &Message, response: &mut Message) -> hid::AppResult {
        let (command, flag) = if command == HidCommand::Vendor(ADMIN) {
            // new mode: first input byte specifies the actual command
            let (command, input) = input_data.split_first().ok_or(Error::InvalidLength)?;
            let command = Command::try_from(*command)?;
            (command, input.first())
        } else {
            // old mode: directly use vendor commands + wink
            (Command::try_from(command)?, input_data.first())
        };
        self.exec(command, flag.copied(), response).map_err(From::from)
    }
}

impl<T, R> iso7816::App for App<T, R>
where T: TrussedClient,
      R: Reboot
{
    // Solo management app
    fn aid(&self) -> iso7816::Aid {
        iso7816::Aid::new(&[ 0xA0, 0x00, 0x00, 0x08, 0x47, 0x00, 0x00, 0x00, 0x01])
    }
}

impl<T, R> apdu::App<{command::SIZE}, {response::SIZE}> for App<T, R>
where T: TrussedClient,
      R: Reboot
{

    fn select(&mut self, _apdu: &ApduCommand, _reply: &mut response::Data) -> apdu::Result {
        Ok(())
    }

    fn deselect(&mut self) {}

    fn call(&mut self, interface: apdu::Interface, apdu: &ApduCommand, reply: &mut response::Data) -> apdu::Result {
        let instruction: u8 = apdu.instruction().into();
        let command = Command::try_from(instruction)?;

        // Reboot may only be called over USB
        if command == Command::Reboot && interface != apdu::Interface::Contact {
            return Err(Status::ConditionsOfUseNotSatisfied);
        }

        self.exec(command, Some(apdu.p1), reply).map_err(From::from)
    }
}

