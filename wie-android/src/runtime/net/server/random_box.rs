//! Random boxes the cash shop sells.
//!
//! Orb stones and relic boxes were opened by the game server when they were bought: the shop delivered
//! their contents, never the box, which the game cannot use. The local server picks the contents from
//! the game's own item tables.

use alloc::{vec, vec::Vec};
use core::ops::RangeInclusive;

use super::super::super::{AndroidContext, hle::vfs_asset};

const ORB_TYPE: u8 = 0x0c;
const NORMAL_ORBS: RangeInclusive<u8> = 1..=13;
/// Hyper orbs come in three tiers of 13, from the weakest up.
const HYPER_ORB_TIERS: [(u8, u32); 3] = [(14, 6), (27, 3), (40, 1)];
const HYPER_ORBS_PER_TIER: u8 = 13;

/// Equipment item tables: weapons, armor and accessories (`item_04` holds only a placeholder).
const EQUIPMENT_TYPES: [u8; 10] = [0, 1, 2, 3, 5, 6, 7, 8, 9, 10];
/// Weapons sold in the cash shop itself never come out of boxes.
const CASH_EQUIPMENT_NAME: &[u8] = "공허".as_bytes();
const EQUIPMENT_STATS_SIZE: usize = 21;
/// Serial numbers of a new item; zero lets the game assign fresh ones.
const NEW_SERIALS: [u8; 24] = [0; 24];
const NO_SOCKET: u8 = 0xff;
const SOCKETS: usize = 5;

/// What one box holds.
enum Contents {
    Orbs(u32),
    HyperOrb,
    /// One piece of equipment of the given grades, up to a required level.
    Equipment {
        grades: fn(u8) -> bool,
        max_level: u8,
    },
}

/// Contents of the cash item `id`, or `None` when it is not a box.
fn contents(id: u8) -> Option<Contents> {
    Some(match id {
        5 => Contents::Orbs(1),
        6 => Contents::Orbs(5),
        7 => Contents::HyperOrb,
        // Small, regular, large and old relic boxes hold ever rarer equipment.
        15 => Contents::Equipment {
            grades: |grade| grade == 0,
            max_level: 30,
        },
        16 => Contents::Equipment {
            grades: |grade| grade <= 3,
            max_level: 45,
        },
        17 => Contents::Equipment {
            grades: |grade| matches!(grade, 3 | 4),
            max_level: 60,
        },
        18 => Contents::Equipment {
            grades: |grade| grade >= 4,
            max_level: u8::MAX,
        },
        _ => return None,
    })
}

struct Random(u32);

impl Random {
    fn below(&mut self, bound: u32) -> u32 {
        // xorshift32
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0 % bound.max(1)
    }
}

/// Serialized `_NET_ITEM_`s inside `quantity` boxes of cash item `id`, or `None` when it is not a box.
pub fn open(context: &AndroidContext, id: u8, quantity: u8) -> Option<Vec<Vec<u8>>> {
    let contents = contents(id)?;
    let seed = context.system.platform().now().raw() as u32;
    let mut random = Random(seed | 1);
    let mut items = Vec::new();
    for _ in 0..quantity {
        match contents {
            Contents::Orbs(count) => {
                for _ in 0..count {
                    let orb = NORMAL_ORBS.start() + random.below(NORMAL_ORBS.len() as u32) as u8;
                    items.push(simple_item(ORB_TYPE, orb));
                }
            }
            Contents::HyperOrb => {
                let total = HYPER_ORB_TIERS.iter().map(|(_, weight)| weight).sum();
                let mut roll = random.below(total);
                let (first, _) = HYPER_ORB_TIERS
                    .into_iter()
                    .find(|(_, weight)| roll.checked_sub(*weight).map(|rest| roll = rest).is_none())?;
                items.push(simple_item(ORB_TYPE, first + random.below(HYPER_ORBS_PER_TIER as u32) as u8));
            }
            Contents::Equipment { grades, max_level } => {
                let candidates = equipment(context)
                    .into_iter()
                    .filter(|piece| grades(piece.grade()) && piece.level() <= max_level)
                    .collect::<Vec<_>>();
                let piece = candidates.get(random.below(candidates.len() as u32) as usize)?;
                items.push(piece.net_item());
            }
        }
    }
    Some(items)
}

/// A stackable item: count 1, type, id and an empty serial buffer.
fn simple_item(item_type: u8, id: u8) -> Vec<u8> {
    let mut item = vec![0, 0, 0, 1, item_type, id];
    item.extend_from_slice(&0u32.to_be_bytes());
    item
}

/// One row of an equipment table.
struct Equipment {
    item_type: u8,
    id: u8,
    /// Little-endian stats as the table stores them: four 16-bit values around two bytes, then the
    /// grade, the required level, a flag and three option (type, value) pairs.
    stats: [u8; EQUIPMENT_STATS_SIZE],
}

impl Equipment {
    fn grade(&self) -> u8 {
        self.stats[12]
    }

    fn level(&self) -> u8 {
        self.stats[13]
    }

    /// The `_NET_ITEM_` the game builds a new item from: big-endian copies of the table stats, no
    /// sockets, and zero for the values the game derives itself.
    fn net_item(&self) -> Vec<u8> {
        let short = |offset: usize| [self.stats[offset + 1], self.stats[offset]];
        let mut item = vec![0, 0, 0, 1, self.item_type, self.id];
        item.extend_from_slice(&0u32.to_be_bytes());
        item.extend_from_slice(&NEW_SERIALS);
        item.extend_from_slice(&short(0));
        item.extend_from_slice(&short(2));
        item.extend_from_slice(&self.stats[4..6]);
        item.extend_from_slice(&short(6));
        item.extend_from_slice(&short(8));
        item.extend_from_slice(&short(10));
        item.extend_from_slice(&self.stats[12..]);
        // Three unused option bytes and the socket count.
        item.extend_from_slice(&[0; 4]);
        item.extend_from_slice(&[NO_SOCKET; SOCKETS]);
        item.extend_from_slice(&[0; 4]);
        item
    }
}

/// Every piece of equipment boxes may hold.
fn equipment(context: &AndroidContext) -> Vec<Equipment> {
    let mut pieces = Vec::new();
    for item_type in EQUIPMENT_TYPES {
        let name = alloc::format!("c/csv/item_{item_type:02}.dat");
        if let Some(table) = vfs_asset(&context.vfs, &name) {
            parse_equipment(item_type, table, &mut pieces);
        }
    }
    pieces
}

/// Rows are `id:i16, icon:i16, name:len8, price:i32, description:len8, stats`, all little-endian.
fn parse_equipment(item_type: u8, table: &[u8], pieces: &mut Vec<Equipment>) -> Option<()> {
    let count = u16::from_le_bytes(table.get(..2)?.try_into().unwrap());
    let mut position = 2;
    for id in 0..count {
        let name_length = *table.get(position + 4)? as usize;
        let name = table.get(position + 5..position + 5 + name_length)?;
        let price_at = position + 5 + name_length;
        let price = i32::from_le_bytes(table.get(price_at..price_at + 4)?.try_into().unwrap());
        let stats_at = price_at + 5 + *table.get(price_at + 4)? as usize;
        let stats = table.get(stats_at..stats_at + EQUIPMENT_STATS_SIZE)?.try_into().unwrap();
        position = stats_at + EQUIPMENT_STATS_SIZE;
        // Priceless rows are unused placeholders.
        if price > 0 && !name.windows(CASH_EQUIPMENT_NAME.len()).any(|part| part == CASH_EQUIPMENT_NAME) {
            pieces.push(Equipment {
                item_type,
                id: id as u8,
                stats,
            });
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    use super::{Equipment, parse_equipment};
    use crate::runtime::net::server::net_item_length;

    #[test]
    fn equipment_becomes_a_complete_net_item() {
        // One row: id 42, icon 199, name "ab", price 200, description "x", then the stats.
        let mut table = vec![1, 0, 42, 0, 199, 0, 2, b'a', b'b', 200, 0, 0, 0, 1, b'x'];
        let stats = [0, 0, 0x21, 1, 1, 0, 20, 0, 36, 0, 14, 0, 3, 12, 0, 0xff, 0, 0xff, 0, 0xff, 0];
        table.extend_from_slice(&stats);
        let mut pieces = Vec::new();
        parse_equipment(2, &table, &mut pieces).unwrap();
        let piece: &Equipment = &pieces[0];
        assert_eq!((piece.id, piece.grade(), piece.level()), (0, 3, 12));

        let item = piece.net_item();
        assert_eq!(net_item_length(&item), Some(item.len()));
        assert_eq!(&item[..6], &[0, 0, 0, 1, 2, 0]);
        // Stats follow the serials big-endian, then the grade and level.
        assert_eq!(&item[34..38], &[0, 0, 0x01, 0x21]);
        assert_eq!(&item[46..48], &[3, 12]);
    }
}
