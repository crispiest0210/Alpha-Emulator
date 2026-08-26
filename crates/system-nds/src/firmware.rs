//! The firmware serial flash on the ARM7's SPI bus.
//!
//! # Why a machine with no firmware image still needs this
//!
//! Direct boot exists so that a game does not need the firmware's *code*. It does not excuse the
//! machine from having the firmware's *chip*. The chip is 256 KiB of serial flash holding the
//! console's own configuration — the owner's name and birthday, the language, the touchscreen
//! calibration, the wifi settings and the MAC address — and a retail game reads it directly over
//! SPI rather than trusting the copy the firmware leaves in RAM.
//!
//! Before this module the bus answered every firmware byte with `0xFF`, on the reasoning that an
//! absent flash is an honest thing to be. It is not honest enough. `0xFF` in the status register
//! means *write in progress*, so the first thing a driver does — wait for the chip to go idle —
//! never finishes. Pokemon Platinum's ARM9 asks its ARM7 to read the settings block, the ARM7's
//! flash driver reports a chip that is permanently busy, the ARM9 gets an error, waits a
//! sixty-fourth of a second, and asks again. Forever, at full speed, with nothing on either
//! screen.
//!
//! # The image is fabricated, and says so
//!
//! There is no firmware image to load, so [`Firmware::new`] builds one: a header whose pointers
//! are consistent, a wifi configuration block, and the two user-settings blocks with correct
//! CRC16s and a valid update counter. What software reads back is a console that has been set up
//! once and never touched since — which is a defensible thing for a console to be, and is what a
//! game's settings-dependent code paths are written against.
//!
//! The parts that are firmware *code* — the boot menu, the two GUI binaries — are left zeroed.
//! Nothing reaches them: a machine that ran them would not be direct-booting in the first place.
//!
//! # Writes land in RAM and stay there
//!
//! The flash is writable and software does write it — the SDK rewrites the settings block to bump
//! its update counter. Those writes are honoured against the in-memory image and are saved in a
//! save state, so a game that writes and reads back sees what it wrote. They are not persisted to
//! disk: the alternative is a per-user firmware file whose contents drift from the fabricated
//! defaults, which is a bigger promise than any of this needs to make.

use core_common::{Savable, StateError, StateReader, StateWriter};

/// The DS's flash is 256 KiB. The DSi's is larger; nothing here is a DSi.
pub const SIZE: usize = 256 * 1024;

/// Where the two user-settings blocks live, as offsets into the image.
///
/// Two copies, written alternately, so a power loss part-way through a write cannot leave the
/// console with no settings at all. Software picks the newer by comparing the update counters at
/// offset `0x70` of each — see [`Firmware::new`].
const SETTINGS_A: usize = 0x3FE00;
const SETTINGS_B: usize = 0x3FF00;

/// One settings block, and the granularity everything about them is expressed in.
const SETTINGS_LEN: usize = 0x100;

/// Where the header keeps `SETTINGS_A / 8`, which is how software finds the blocks at all.
const SETTINGS_POINTER: usize = 0x20;

/// Where the wifi configuration block starts, and where the header points at it in 8-byte units.
const WIFI_CONFIG: usize = 0x21000;
const WIFI_CONFIG_POINTER: usize = 0x00;

/// Size of the wifi configuration block that its own CRC16 covers.
const WIFI_CONFIG_LEN: usize = 0x138;

/// A JEDEC identity in the shape a real DS flash reports: manufacturer, type, and a capacity code
/// that is the log2 of the byte count.
const JEDEC_ID: [u8; 3] = [0x20, 0x40, 0x12];

/// Status register bits. Only these two are storage; the rest read as zero.
mod status {
    /// Write in progress. Never set here: writes complete inside the transfer that requests
    /// them, so there is no moment at which the chip is busy for software to observe. It is
    /// named rather than merely absent because it is the bit the whole module turns on — a chip
    /// that reports it forever is a chip no driver will ever use.
    #[allow(dead_code)]
    pub const WIP: u8 = 1 << 0;
    /// Write enable latch, set by `WREN` and cleared by `WRDI` and by every completed write.
    pub const WEL: u8 = 1 << 1;
}

/// SPI flash opcodes, as the DS's chip implements them.
mod op {
    pub const WRITE_DISABLE: u8 = 0x04;
    pub const READ_STATUS: u8 = 0x05;
    pub const WRITE_ENABLE: u8 = 0x06;
    pub const READ: u8 = 0x03;
    pub const FAST_READ: u8 = 0x0B;
    pub const PAGE_WRITE: u8 = 0x0A;
    pub const PAGE_PROGRAM: u8 = 0x02;
    pub const PAGE_ERASE: u8 = 0xDB;
    pub const SECTOR_ERASE: u8 = 0xD8;
    pub const READ_ID: u8 = 0x9F;
    pub const DEEP_POWER_DOWN: u8 = 0xB9;
    pub const RELEASE_POWER_DOWN: u8 = 0xAB;
}

/// A page, which is what `PAGE_PROGRAM` wraps within and what `PAGE_ERASE` clears.
const PAGE: usize = 0x100;
/// A sector, which is what `SECTOR_ERASE` clears.
const SECTOR: usize = 0x10000;

/// The firmware flash chip: an image, and where a transfer has got to within one command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Firmware {
    image: Vec<u8>,
    /// The opcode of the command in progress, or `None` between commands.
    command: Option<u8>,
    /// How many bytes of this command have been shifted in, the opcode included.
    position: u32,
    /// The address the command's three address bytes have built up so far.
    address: u32,
    status: u8,
}

impl Default for Firmware {
    fn default() -> Self {
        Self::new()
    }
}

impl Firmware {
    /// A fabricated but self-consistent firmware image. See the module docs.
    pub fn new() -> Self {
        let mut image = vec![0u8; SIZE];
        write_header(&mut image);
        write_wifi_config(&mut image);
        // Both blocks are written, and the counters differ by one, so the "which is newer?"
        // comparison every reader performs has an answer rather than a tie.
        write_settings(&mut image, SETTINGS_A, 0);
        write_settings(&mut image, SETTINGS_B, 1);
        Self {
            image,
            command: None,
            position: 0,
            address: 0,
            status: 0,
        }
    }

    /// The bytes, for a test or a diagnostic that wants to check what was fabricated.
    pub fn image(&self) -> &[u8] {
        &self.image
    }

    /// The settings block software would take as the current one.
    ///
    /// Two blocks are kept and written alternately, and the newer is the one whose update counter
    /// is one greater than the other's, modulo the seven bits the counter occupies. Direct boot
    /// copies the answer into RAM, exactly as the firmware does — see
    /// `NdsSystem::write_user_settings` — so the RAM copy and the flash agree about which
    /// settings the console has.
    pub fn current_user_settings(&self) -> &[u8] {
        let counter =
            |at: usize| u16::from_le_bytes([self.image[at + 0x70], self.image[at + 0x71]]) & 0x7F;
        let at = if (counter(SETTINGS_A) + 1) & 0x7F == counter(SETTINGS_B) {
            SETTINGS_B
        } else {
            SETTINGS_A
        };
        &self.image[at..at + SETTINGS_LEN]
    }

    /// Deselecting the chip ends whatever command was in progress.
    ///
    /// This is the whole of the chip's framing: a command runs for as long as the select line is
    /// held, and the next transfer after a release is an opcode rather than more of the last one.
    /// Miss it and a driver that reads two blocks in a row gets the second one from wherever the
    /// first left off.
    pub fn deselect(&mut self) {
        self.command = None;
        self.position = 0;
        self.address = 0;
    }

    /// Shift one byte in, and return the byte shifted out at the same time.
    pub fn transfer(&mut self, byte: u8) -> u8 {
        let Some(command) = self.command else {
            self.command = Some(byte);
            self.position = 1;
            self.address = 0;
            return self.begin(byte);
        };
        let position = self.position;
        self.position += 1;
        match command {
            op::READ
            | op::FAST_READ
            | op::PAGE_WRITE
            | op::PAGE_PROGRAM
            | op::PAGE_ERASE
            | op::SECTOR_ERASE => self.addressed(command, position, byte),
            // The status stays available for as long as the driver keeps clocking, which is how
            // a wait loop is written: select once, read until the busy bit clears.
            op::READ_STATUS => self.status,
            op::READ_ID => *JEDEC_ID.get(position as usize - 1).unwrap_or(&0),
            _ => 0,
        }
    }

    /// The opcodes that act the moment they arrive, with no address or data behind them.
    fn begin(&mut self, command: u8) -> u8 {
        match command {
            op::WRITE_ENABLE => self.status |= status::WEL,
            op::WRITE_DISABLE => self.status &= !status::WEL,
            // Power-down state is not modelled: what it changes is how much current the chip
            // draws, and the only software-visible part — that reads return nothing until
            // released — would turn a driver being polite into a driver reading zeros.
            op::DEEP_POWER_DOWN | op::RELEASE_POWER_DOWN => {}
            _ => {}
        }
        // The byte shifted out alongside an opcode is not defined by the chip; zero is what a
        // released data line reads as.
        0
    }

    /// The body of the commands that take a three-byte address first.
    fn addressed(&mut self, command: u8, position: u32, byte: u8) -> u8 {
        // Bytes one to three are the address, most significant first.
        if position <= 3 {
            self.address = (self.address << 8) | byte as u32;
            if position == 3 {
                self.on_address_complete(command);
            }
            return 0;
        }
        // `FAST_READ` clocks one dummy byte between the address and the data.
        if command == op::FAST_READ && position == 4 {
            return 0;
        }
        match command {
            op::READ | op::FAST_READ => {
                let at = self.address as usize % SIZE;
                self.address = self.address.wrapping_add(1);
                self.image[at]
            }
            op::PAGE_WRITE | op::PAGE_PROGRAM => {
                if self.status & status::WEL != 0 {
                    // A program wraps within its page rather than running on into the next one,
                    // which is what a driver writing a 256-byte block relies on to stay inside it.
                    let base = self.address as usize & !(PAGE - 1);
                    let at = base | (self.address as usize & (PAGE - 1));
                    // `PAGE_PROGRAM` can only clear bits; `PAGE_WRITE` replaces the byte.
                    self.image[at % SIZE] = if command == op::PAGE_PROGRAM {
                        self.image[at % SIZE] & byte
                    } else {
                        byte
                    };
                }
                self.address = (self.address & !(PAGE as u32 - 1))
                    | (self.address.wrapping_add(1) & (PAGE as u32 - 1));
                0
            }
            _ => 0,
        }
    }

    /// Erases happen the moment their address is complete; there is no data phase behind them.
    fn on_address_complete(&mut self, command: u8) {
        let span = match command {
            op::PAGE_ERASE => PAGE,
            op::SECTOR_ERASE => SECTOR,
            _ => return,
        };
        if self.status & status::WEL == 0 {
            return;
        }
        let base = self.address as usize & !(span - 1);
        for byte in &mut self.image[base % SIZE..(base % SIZE) + span] {
            *byte = 0xFF;
        }
        self.status &= !status::WEL;
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }
}

/// The header's pointers, which are the only part of it software follows.
///
/// Everything a game reads it reaches through one of these two, so a header that is zero is a
/// firmware with no settings and no wifi configuration in it however correct the blocks
/// themselves are.
fn write_header(image: &mut [u8]) {
    put16(image, WIFI_CONFIG_POINTER, (WIFI_CONFIG / 8) as u16);
    put16(image, SETTINGS_POINTER, (SETTINGS_A / 8) as u16);
    // Part 3's CRC16, over the wifi configuration block. Filled in by `write_wifi_config`, which
    // is the only thing that knows what it covers.
}

/// The wifi configuration block, and the CRC16 in the header that covers it.
///
/// Nothing here has a radio behind it — see the crate docs on wifi being out of scope — but the
/// block still has to be present and checksummed, because a game reads the console's MAC address
/// and its enabled-channel mask out of it before it discovers there is no radio.
fn write_wifi_config(image: &mut [u8]) {
    put16(image, WIFI_CONFIG, WIFI_CONFIG_LEN as u16);
    // A locally administered MAC address: bit 1 of the first octet set marks it as one that was
    // assigned by whoever is running it rather than bought from the IEEE, which is exactly what
    // a fabricated address is.
    const MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
    image[WIFI_CONFIG + 0x36..WIFI_CONFIG + 0x3C].copy_from_slice(&MAC);
    // Channels 1-13, which is every channel the DS's radio has.
    put16(image, WIFI_CONFIG + 0x3C, 0x3FFE);
    let crc = crate::bios::crc16(0x0000, &image[WIFI_CONFIG..WIFI_CONFIG + WIFI_CONFIG_LEN]);
    put16(image, 0x02, crc);
}

/// One user-settings block, with the update counter and CRC16 that make it readable.
///
/// The layout is the console's own settings screen, field for field. Only three of them decide
/// anything here — the touchscreen calibration, which has to invert what [`crate::input`]
/// reports; the language; and the counter that says which of the two blocks is current — and the
/// rest are the defaults a console leaves its settings at.
fn write_settings(image: &mut [u8], at: usize, counter: u16) {
    let block = &mut image[at..at + SETTINGS_LEN];
    block.fill(0);
    // Version 5, which is what a DS Lite writes and what every reader accepts.
    block[0x00] = 5;
    // Favourite colour, birthday month and day: the settings screen's own defaults.
    block[0x02] = 0;
    block[0x03] = 1;
    block[0x04] = 1;
    // An empty nickname and message. Both are length-prefixed UTF-16 and both are allowed to be
    // empty; a console whose owner skipped the setup screen has exactly this.
    block[0x1A] = 0;
    block[0x50] = 0;

    // The touchscreen calibration, which is the inverse of what the controller reports. Two
    // points, one at the origin and one at the far corner, give the linear mapping software
    // expects — the same pair `NdsSystem::write_user_settings` puts in RAM, and for the same
    // reason: the numbers on both ends of the mapping are ours to choose, but they have to be
    // the same numbers. See [`crate::input::RAW_PER_PIXEL`].
    let raw = crate::input::RAW_PER_PIXEL;
    put16(block, 0x58, 0);
    put16(block, 0x5A, 0);
    block[0x5C] = 0;
    block[0x5D] = 0;
    put16(block, 0x5E, 255 * raw);
    put16(block, 0x60, 191 * raw);
    block[0x62] = 255;
    block[0x63] = 191;

    // Language 1, English. The other bits in this halfword select things this machine has no
    // opinion about — the GBA-mode screen, the backlight level — and are left at zero rather than
    // guessed at.
    put16(block, 0x64, 0x0001);
    // The RTC offset, which is how far the clock has been adjusted by hand. Never, here.
    put32(block, 0x68, 0);
    // The update counter. Software takes the block whose counter is one greater than the other's,
    // modulo 128, so these two must differ by exactly one.
    put16(block, 0x70, counter & 0x7F);
    // Over the block up to but not including the counter and this checksum, and started from
    // all ones — which is what software passes to `GetCrc16` when it checks the block, so the two
    // agree by sharing the routine rather than by two implementations happening to match.
    let crc = crate::bios::crc16(0xFFFF, &block[0x00..0x70]);
    put16(block, 0x72, crc);
}

fn put16(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn put32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

impl Savable for Firmware {
    fn save(&self, w: &mut StateWriter) {
        for byte in &self.image {
            w.write_u8(*byte);
        }
        w.write_u8(self.command.unwrap_or(0));
        w.write_bool(self.command.is_some());
        w.write_u32(self.position);
        w.write_u32(self.address);
        w.write_u8(self.status);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        for byte in &mut self.image {
            *byte = r.read_u8()?;
        }
        let command = r.read_u8()?;
        self.command = r.read_bool()?.then_some(command);
        self.position = r.read_u32()?;
        self.address = r.read_u32()?;
        self.status = r.read_u8()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
