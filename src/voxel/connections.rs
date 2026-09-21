//! Terrain beyond the engine's short, live strip of an adjoining map.
use gba::bus::Bus;

fn word(rom: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        rom.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn ptr(rom: &[u8], at: usize) -> Option<usize> {
    let p = word(rom, at)?.checked_sub(0x0800_0000)? as usize;
    (p < rom.len()).then_some(p)
}

#[derive(Clone)]
pub(super) struct Link {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    data: usize,
    primary: bool,
    secondary: bool,
}

impl Link {
    // The outer Option distinguishes no connection here from terrain whose
    // tileset is unavailable. Do not replace the latter with border trees.
    pub(super) fn at(&self, rom: &[u8], x: i32, y: i32) -> Option<Option<u16>> {
        let (x, y) = (x - self.x, y - self.y);
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            return None;
        }
        let at = self.data + ((y * self.w + x) * 2) as usize;
        let e = u16::from_le_bytes([rom[at], rom[at + 1]]);
        let id = e & 0x3ff;
        Some(
            (id != 0x3ff
                && if id < 0x280 {
                    self.primary
                } else {
                    self.secondary
                })
            .then_some(e),
        )
    }
}

#[derive(Default)]
struct Cache {
    key: Option<(usize, usize, [u8; 16], u8, u8)>,
    links: Vec<Link>,
}

thread_local! {
    static CACHE: std::cell::RefCell<Cache> = std::cell::RefCell::new(Cache::default());
}

pub(super) fn reset() {
    CACHE.with(|c| *c.borrow_mut() = Cache::default());
}

// Locate the map-group table from the current header and its saved bank/index.
// This avoids a ROM-revision-specific address. Require an exact match of all
// four header pointers before following either level of pointer tables.
fn groups(rom: &[u8], header: &[u8; 16], group: u8, number: u8) -> Option<usize> {
    let h = (0..rom.len().saturating_sub(28))
        .step_by(4)
        .find(|&at| rom.get(at..at + 16) == Some(header.as_slice()))?;
    let target = h as u32 + 0x0800_0000;
    for reference in (0..rom.len().saturating_sub(4)).step_by(4) {
        if word(rom, reference) != Some(target) {
            continue;
        }
        let Some(bank) = reference.checked_sub(number as usize * 4) else {
            continue;
        };
        let bank_ptr = bank as u32 + 0x0800_0000;
        for at in (0..rom.len().saturating_sub(4)).step_by(4) {
            if word(rom, at) == Some(bank_ptr)
                && let Some(root) = at.checked_sub(group as usize * 4)
            {
                return Some(root);
            }
        }
    }
    None
}

fn read(rom: &[u8], header: &[u8; 16], group: u8, number: u8) -> Option<Vec<Link>> {
    let layout = word(header, 0)?.checked_sub(0x0800_0000)? as usize;
    let list_header = word(header, 12)?.checked_sub(0x0800_0000)? as usize;
    let count = word(rom, list_header)?;
    if count > 16 {
        return None;
    }
    if count == 0 {
        return Some(Vec::new());
    }
    let list = ptr(rom, list_header + 4)?;
    let root = groups(rom, header, group, number)?;
    let (w, h) = (word(rom, layout)? as i32, word(rom, layout + 4)? as i32);
    let mut result = Vec::new();
    for i in 0..count as usize {
        let at = list + i * 12;
        let direction = word(rom, at)?;
        let offset = word(rom, at + 4)? as i32;
        let group = *rom.get(at + 8)? as usize;
        let number = *rom.get(at + 9)? as usize;
        let bank = ptr(rom, root + group * 4)?;
        let other_header = ptr(rom, bank + number * 4)?;
        let other = ptr(rom, other_header)?;
        let (ow, oh) = (word(rom, other)? as i32, word(rom, other + 4)? as i32);
        if !(1..=1024).contains(&ow)
            || !(1..=1024).contains(&oh)
            || !(-1024..=1024).contains(&offset)
        {
            return None;
        }
        let (x, y) = match direction {
            1 => (offset, h),
            2 => (offset, -oh),
            3 => (-ow, offset),
            4 => (w, offset),
            _ => continue,
        };
        let data = ptr(rom, other + 12)?;
        rom.get(data..data + (ow * oh * 2) as usize)?;
        result.push(Link {
            x,
            y,
            w: ow,
            h: oh,
            data,
            primary: word(rom, layout + 16) == word(rom, other + 16),
            secondary: word(rom, layout + 20) == word(rom, other + 20),
        });
    }
    Some(result)
}

pub(super) fn read_cached(bus: &Bus, save: usize) -> Vec<Link> {
    let header: [u8; 16] = bus.ewram[0x36dfc..0x36e0c].try_into().unwrap();
    let Some(&group) = bus.ewram.get(save + 4) else {
        return Vec::new();
    };
    let Some(&number) = bus.ewram.get(save + 5) else {
        return Vec::new();
    };
    let key = (
        bus.rom.as_ptr() as usize,
        bus.rom.len(),
        header,
        group,
        number,
    );
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.key.as_ref() != Some(&key) {
            c.links = read(&bus.rom, &header, group, number).unwrap_or_default();
            c.key = Some(key);
        }
        c.links.clone()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_connections_and_places_all_four_directions_with_offsets() {
        let mut rom = vec![0u8; 0x1000];
        let put = |rom: &mut [u8], at: usize, value: u32| {
            rom[at..at + 4].copy_from_slice(&value.to_le_bytes());
        };
        put(&mut rom, 0x44, 0x0800_0080); // bank 1
        put(&mut rom, 0x48, 0x0800_0090); // bank 2
        put(&mut rom, 0x88, 0x0800_0700); // current map 1.2
        put(&mut rom, 0x700, 0x0800_0100);
        put(&mut rom, 0x70c, 0x0800_0880);
        put(&mut rom, 0x100, 10);
        put(&mut rom, 0x104, 12);
        put(&mut rom, 0x110, 0x0800_0e00);
        put(&mut rom, 0x114, 0x0800_0e40);
        put(&mut rom, 0x880, 4);
        put(&mut rom, 0x884, 0x0800_0900);
        for (i, offset) in [-2i32, 3, -1, 2].into_iter().enumerate() {
            let layout = 0x200 + i * 0x100;
            let header = 0x600 + i * 28;
            let data = 0xa00 + i * 80;
            put(&mut rom, 0x90 + i * 4, 0x0800_0000 + header as u32);
            put(&mut rom, header, 0x0800_0000 + layout as u32);
            put(&mut rom, layout, 4);
            put(&mut rom, layout + 4, 5);
            put(&mut rom, layout + 12, 0x0800_0000 + data as u32);
            put(&mut rom, layout + 16, 0x0800_0e00);
            put(&mut rom, layout + 20, 0x0800_0e40);
            put(&mut rom, 0x900 + i * 12, i as u32 + 1);
            put(&mut rom, 0x904 + i * 12, offset as u32);
            rom[0x908 + i * 12] = 2;
            rom[0x909 + i * 12] = i as u8;
            for tile in 0..20 {
                rom[data + tile * 2..data + tile * 2 + 2]
                    .copy_from_slice(&(0x3000u16 + tile as u16).to_le_bytes());
            }
        }
        let header = rom[0x700..0x710].try_into().unwrap();
        assert_eq!(groups(&rom, &header, 1, 2), Some(0x40));
        let links = read(&rom, &header, 1, 2).unwrap();
        assert_eq!(links.len(), 4);
        for (link, (x, y)) in links.iter().zip([(-2, 12), (3, -5), (-4, -1), (10, 2)]) {
            assert_eq!(link.at(&rom, x, y), Some(Some(0x3000)));
            assert_eq!(link.at(&rom, x + 3, y + 4), Some(Some(0x3013)));
            assert_eq!(link.at(&rom, x - 1, y), None);
            assert_eq!(link.at(&rom, x + 4, y), None);
            assert_eq!(link.at(&rom, x, y + 5), None);
        }
        assert!(read(&rom[..0x940], &header, 1, 2).is_none());
    }

    #[test]
    fn unavailable_tilesets_and_undefined_cells_are_not_border_scenery() {
        let rom = [0x01, 0x30, 0x80, 0x32, 0xff, 0x33];
        let mut link = Link {
            x: 0,
            y: 0,
            w: 3,
            h: 1,
            data: 0,
            primary: true,
            secondary: false,
        };
        assert_eq!(link.at(&rom, 0, 0), Some(Some(0x3001)));
        assert_eq!(link.at(&rom, 1, 0), Some(None));
        assert_eq!(link.at(&rom, 2, 0), Some(None));
        link.primary = false;
        link.secondary = true;
        assert_eq!(link.at(&rom, 0, 0), Some(None));
        assert_eq!(link.at(&rom, 1, 0), Some(Some(0x3280)));
    }
}
