//! Replies of the local game server.
//!
//! Requests and replies share one layout: a [`HEADER_SIZE`]-byte header, then big-endian fields.
//! Every reply starts with a result code (0 is success) and a message the game may show.

mod random_box;

use alloc::{vec, vec::Vec};

use encoding_rs::EUC_KR;

use super::{
    super::{AndroidContext, hle::vfs_asset},
    HEADER_SIZE,
};

/// Packet categories (`NETWORK::RecvParsering`).
const CATEGORY_SYSTEM: u32 = 0;
const CATEGORY_LOGIN: u32 = 1;
const CATEGORY_STORAGE: u32 = 4;
const CATEGORY_SHOP: u32 = 6;
const CATEGORY_NOTICE: u32 = 7;
const CATEGORY_EVENT: u32 = 9;

/// System commands.
const SYSTEM_KEEP_ALIVE: u32 = 2;

/// Login commands.
const LOGIN_CONNECT: u32 = 1;
const LOGIN_SMS_CONSENT: u32 = 2;
const LOGIN_NICKNAME: u32 = 3;
const LOGIN_ACCOUNT: u32 = 5;

/// Network storage commands.
const STORAGE_DEPOSIT: u32 = 1;
const STORAGE_DEPOSIT_MONEY: u32 = 2;
const STORAGE_WITHDRAW: u32 = 3;
const STORAGE_LIST: u32 = 6;
const STORAGE_MONEY: u32 = 7;
const STORAGE_DEPOSIT_QUICK: u32 = 10;
const STORAGE_DEPOSITED: u32 = 11;

/// Cash shop commands.
const SHOP_ORDER: u32 = 2;
const SHOP_DELIVER: u32 = 3;
const SHOP_DELIVER_PAID: u32 = 5;
const SHOP_RECEIVED: u32 = 7;

/// Notice and event commands.
const NOTICE_LIST: u32 = 1;
const EVENT_LIST: u32 = 1;

/// Seconds between the game's keep-alive packets.
const KEEP_ALIVE_SECONDS: u32 = 60;
/// Account IDs handed out by the local server; the game stores them in its save.
const ACCOUNT_ID: u64 = 1;
/// Network nickname of the local player (at most 15 bytes in EUC-KR).
const NICKNAME: &str = "용사";
const NOTICE: &str = "오프라인 서버에 접속했습니다.";
const UNSUPPORTED_MESSAGE: &str = "오프라인에서는 사용할 수 없는 기능입니다.";

/// Product table of the network cash shop: `count:i16le`, then per product `key:i16le,
/// product:u8, icon:i16le, name:len8+bytes, item:u8, quantity:u8, description:len8+bytes,
/// prices:3*i16le`.
const CASH_PRODUCTS: &str = "c/csv/cash_network_android.dat";
/// Item type of cash shop goods in `_NET_ITEM_`.
const CASH_ITEM_TYPE: u8 = 0x12;
/// Items of this type and below are equipment and carry the long `_NET_ITEM_` tail.
const LAST_EQUIPMENT_TYPE: u8 = 0x0a;
/// Size of the equipment tail of `_NET_ITEM_`: 3 longs, 2 shorts, 2 bytes, 3 shorts, 18 bytes, 2 shorts.
const EQUIPMENT_TAIL_SIZE: usize = 3 * 8 + 2 * 2 + 2 + 3 * 2 + 18 + 2 * 2;

/// Contents of the network storage, kept in the app's persistent files.
const STORAGE_FILE: &str = "files/network_storage.dat";
/// Slots of the network storage; the game shows them in tabs of 16.
const STORAGE_SLOTS: u32 = 48;
const STORAGE_FULL_MESSAGE: &str = "창고가 가득 찼습니다.";

/// Builds a reply with the request's tag.
struct Reply {
    data: Vec<u8>,
}

impl Reply {
    fn new(request: &[u8], category: u32, command: u32) -> Self {
        let mut data = vec![0; 4];
        data.extend_from_slice(&request[4..12]);
        data.extend_from_slice(&category.to_be_bytes());
        data.extend_from_slice(&command.to_be_bytes());
        Self { data }
    }

    fn byte(mut self, value: u8) -> Self {
        self.data.push(value);
        self
    }

    fn int(mut self, value: u32) -> Self {
        self.data.extend_from_slice(&value.to_be_bytes());
        self
    }

    fn long(mut self, value: u64) -> Self {
        self.data.extend_from_slice(&value.to_be_bytes());
        self
    }

    fn raw(mut self, value: &[u8]) -> Self {
        self.data.extend_from_slice(value);
        self
    }

    fn buf(self, value: &[u8]) -> Self {
        self.int(value.len() as u32).raw(value)
    }

    fn text(self, value: &str) -> Self {
        self.buf(&EUC_KR.encode(value).0)
    }

    /// Result code 0 with an empty message, which successful replies start with.
    fn success(request: &[u8], category: u32, command: u32) -> Self {
        Self::new(request, category, command).int(0).buf(&[])
    }

    /// A failure the game reports in a popup.
    fn failure(request: &[u8], category: u32, command: u32, message: &str) -> Self {
        Self::new(request, category, command).int(1).text(message)
    }

    fn finish(mut self) -> Vec<u8> {
        let length = self.data.len() as u32;
        self.data[..4].copy_from_slice(&length.to_be_bytes());
        self.data
    }
}

/// Reads big-endian fields of a request body.
struct Body<'a> {
    data: &'a [u8],
}

impl<'a> Body<'a> {
    fn int(&mut self) -> Option<u32> {
        let (value, rest) = self.data.split_first_chunk::<4>()?;
        self.data = rest;
        Some(u32::from_be_bytes(*value))
    }

    /// One serialized `_NET_ITEM_`, kept as raw bytes.
    fn item(&mut self) -> Option<&'a [u8]> {
        let length = net_item_length(self.data)?;
        let (item, rest) = self.data.split_at_checked(length)?;
        self.data = rest;
        Some(item)
    }
}

/// Length of a serialized `_NET_ITEM_`: count, type, id, serial buffer, and the equipment tail.
pub fn net_item_length(data: &[u8]) -> Option<usize> {
    let count = i32::from_be_bytes(data.get(..4)?.try_into().unwrap());
    if count <= 0 {
        return Some(4);
    }
    let item_type = *data.get(4)?;
    let serial = i32::from_be_bytes(data.get(6..10)?.try_into().unwrap()).max(0) as usize;
    let tail = if item_type <= LAST_EQUIPMENT_TYPE { EQUIPMENT_TAIL_SIZE } else { 0 };
    let length = 10 + serial + tail;
    (data.len() >= length).then_some(length)
}

/// One stored item: its storage slot, its state flag and its serialized `_NET_ITEM_`.
struct StoredItem {
    slot: u32,
    state: u8,
    item: Vec<u8>,
}

#[derive(Default)]
struct Storage {
    money: u32,
    items: Vec<StoredItem>,
}

impl Storage {
    async fn load(context: &AndroidContext) -> Self {
        let filesystem = context.system.filesystem();
        let Some(size) = filesystem.size(STORAGE_FILE).await else {
            return Self::default();
        };
        let mut data = vec![0; size];
        filesystem.read(STORAGE_FILE, 0, size, &mut data).await;
        Self::decode(&data).unwrap_or_default()
    }

    /// `money:u32, count:u32`, then per item `slot:u32, state:u8, length:u32, item`, all big-endian.
    fn decode(data: &[u8]) -> Option<Self> {
        let mut body = Body { data };
        let money = body.int()?;
        let count = body.int()?;
        let mut items = Vec::new();
        for _ in 0..count {
            let slot = body.int()?;
            let (&state, rest) = body.data.split_first()?;
            body.data = rest;
            let length = body.int()? as usize;
            let (item, rest) = body.data.split_at_checked(length)?;
            body.data = rest;
            items.push(StoredItem {
                slot,
                state,
                item: item.to_vec(),
            });
        }
        Some(Self { money, items })
    }

    fn encode(&self, data: &mut Vec<u8>) {
        data.extend_from_slice(&self.money.to_be_bytes());
        data.extend_from_slice(&(self.items.len() as u32).to_be_bytes());
        for stored in &self.items {
            data.extend_from_slice(&stored.slot.to_be_bytes());
            data.push(stored.state);
            data.extend_from_slice(&(stored.item.len() as u32).to_be_bytes());
            data.extend_from_slice(&stored.item);
        }
    }

    async fn save(&self, context: &AndroidContext) {
        let mut data = Vec::new();
        self.encode(&mut data);
        let filesystem = context.system.filesystem();
        filesystem.truncate(STORAGE_FILE, 0).await;
        filesystem.write(STORAGE_FILE, 0, &data).await;
    }

    fn free_slot(&self) -> Option<u32> {
        (0..STORAGE_SLOTS).find(|slot| self.items.iter().all(|stored| stored.slot != *slot))
    }
}

/// Cash item and quantity a product delivers.
fn cash_product(vfs: &[u8], product: u32) -> Option<(u8, u8)> {
    let table = vfs_asset(vfs, CASH_PRODUCTS)?;
    let count = u16::from_le_bytes(table.get(..2)?.try_into().unwrap());
    let mut position = 2usize;
    for _ in 0..count {
        let entry_product = *table.get(position + 2)?;
        position += 6 + *table.get(position + 5)? as usize;
        let (item, quantity) = (*table.get(position)?, *table.get(position + 1)?);
        position += 3 + *table.get(position + 2)? as usize + 6;
        if entry_product as u32 == product {
            return Some((item, quantity));
        }
    }
    None
}

/// Replies the server sends for one packet from the game, or `None` to refuse it.
pub async fn answer(context: &AndroidContext, packet: &[u8]) -> Option<Vec<Vec<u8>>> {
    if packet.len() < HEADER_SIZE {
        return None;
    }
    let category = u32::from_be_bytes(packet[12..16].try_into().unwrap());
    let command = u32::from_be_bytes(packet[16..20].try_into().unwrap());
    // Keep-alives need no reply.
    if (category, command) == (CATEGORY_SYSTEM, SYSTEM_KEEP_ALIVE) {
        return Some(Vec::new());
    }
    // Unsupported services and malformed requests report an error the game shows in a popup.
    let reply = respond(context, packet, category, command)
        .await
        .unwrap_or_else(|| Reply::failure(packet, category, command, UNSUPPORTED_MESSAGE));
    Some(vec![reply.finish()])
}

async fn respond(context: &AndroidContext, packet: &[u8], category: u32, command: u32) -> Option<Reply> {
    let mut body = Body {
        data: &packet[HEADER_SIZE..],
    };
    let success = || Reply::success(packet, category, command);

    Some(match (category, command) {
        (CATEGORY_LOGIN, LOGIN_CONNECT) => success().int(KEEP_ALIVE_SECONDS),
        (CATEGORY_LOGIN, LOGIN_SMS_CONSENT) => success(),
        (CATEGORY_LOGIN, LOGIN_NICKNAME) => success().text(NICKNAME),
        (CATEGORY_LOGIN, LOGIN_ACCOUNT) => success().long(ACCOUNT_ID).long(ACCOUNT_ID),

        // Every slot is listed; empty ones carry an item count of 0.
        (CATEGORY_STORAGE, STORAGE_LIST) => {
            let storage = Storage::load(context).await;
            let mut reply = success().int(STORAGE_SLOTS);
            for slot in 0..STORAGE_SLOTS {
                reply = match storage.items.iter().find(|stored| stored.slot == slot) {
                    Some(stored) => reply.int(slot).byte(stored.state).raw(&stored.item),
                    None => reply.int(slot).byte(0).int(0),
                };
            }
            reply
        }
        (CATEGORY_STORAGE, STORAGE_MONEY) => success().int(Storage::load(context).await.money),
        (CATEGORY_STORAGE, STORAGE_DEPOSIT | STORAGE_DEPOSIT_QUICK) => {
            let item = body.item()?;
            let mut storage = Storage::load(context).await;
            let Some(slot) = storage.free_slot() else {
                return Some(Reply::failure(packet, category, command, STORAGE_FULL_MESSAGE));
            };
            tracing::debug!("net: storing item in slot {slot}");
            storage.items.push(StoredItem {
                slot,
                state: 0,
                item: item.to_vec(),
            });
            storage.save(context).await;
            success()
        }
        (CATEGORY_STORAGE, STORAGE_DEPOSITED) => success(),
        (CATEGORY_STORAGE, STORAGE_DEPOSIT_MONEY) => {
            let amount = body.int()?;
            let mut storage = Storage::load(context).await;
            storage.money = storage.money.saturating_add(amount);
            storage.save(context).await;
            success()
        }
        (CATEGORY_STORAGE, STORAGE_WITHDRAW) => {
            let (slot, count) = (body.int()?, body.int()?);
            let mut storage = Storage::load(context).await;
            let index = storage.items.iter().position(|stored| stored.slot == slot)?;
            let stored = &mut storage.items[index];
            let stored_count = u32::from_be_bytes(stored.item[..4].try_into().unwrap());
            let taken = count.clamp(1, stored_count.max(1));
            tracing::debug!("net: taking {taken} of {stored_count} from slot {slot}");
            let mut item = stored.item.clone();
            item[..4].copy_from_slice(&taken.to_be_bytes());
            if taken < stored_count {
                stored.item[..4].copy_from_slice(&(stored_count - taken).to_be_bytes());
            } else {
                storage.items.remove(index);
            }
            storage.save(context).await;
            success().raw(&item)
        }

        (CATEGORY_SHOP, SHOP_ORDER) => success(),
        // Delivers the product's goods as one `_NET_ITEM_` (count, type, id, empty serial).
        (CATEGORY_SHOP, SHOP_DELIVER | SHOP_DELIVER_PAID) => {
            let product = body.int()?;
            let (item, quantity) = cash_product(&context.vfs, product)?;
            tracing::debug!("net: delivering cash product {product}: item {item} x{quantity}");
            match random_box::open(context, item, quantity) {
                Some(contents) => contents.iter().fold(success().int(contents.len() as u32), |reply, item| reply.raw(item)),
                None => success().int(1).int(quantity as u32).byte(CASH_ITEM_TYPE).byte(item).buf(&[]),
            }
        }
        (CATEGORY_SHOP, SHOP_RECEIVED) => success(),

        (CATEGORY_NOTICE, NOTICE_LIST) => success().text(NOTICE),
        // No running events: a flag, an empty banner and zero entries.
        (CATEGORY_EVENT, EVENT_LIST) => success().byte(0).buf(&[]).int(0),

        // Trading with other players, mail, rankings and the like have no offline counterpart.
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::{Storage, StoredItem, net_item_length};

    #[test]
    fn net_item_length_covers_equipment_tail() {
        // count 1, type 5 (equipment), id 0, 2-byte serial, then the 58-byte tail.
        let mut item = vec![0, 0, 0, 1, 5, 0, 0, 0, 0, 2, b'a', b'b'];
        item.extend_from_slice(&[0; 58]);
        assert_eq!(net_item_length(&item), Some(item.len()));
        // Cash goods (type 0x12) stop after the serial.
        assert_eq!(net_item_length(&[0, 0, 0, 20, 0x12, 34, 0, 0, 0, 0]), Some(10));
        // Truncated equipment is rejected.
        assert_eq!(net_item_length(&item[..20]), None);
    }

    #[test]
    fn storage_round_trips() {
        let storage = Storage {
            money: 1234,
            items: vec![StoredItem {
                slot: 3,
                state: 0,
                item: vec![0, 0, 0, 20, 0x12, 34, 0, 0, 0, 0],
            }],
        };
        let mut data = vec![];
        storage.encode(&mut data);
        let decoded = Storage::decode(&data).unwrap();
        assert_eq!(decoded.money, 1234);
        assert_eq!(decoded.items.len(), 1);
        assert_eq!((decoded.items[0].slot, decoded.items[0].state), (3, 0));
        assert_eq!(decoded.items[0].item, storage.items[0].item);
    }
}
