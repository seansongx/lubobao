//! Douyin `a_bogus` request signature, ported from StreamGet v4.0.10
//! (`streamget/platforms/douyin/ab_sign.py`), Copyright (c) 2025 Hmily,
//! MIT License; see `THIRD_PARTY_NOTICES.md`. The Python version is pure
//! computation (SM3 + RC4 + custom base64), so no JavaScript runtime is needed.
//!
//! Every intermediate "string" in the Python code only holds code points
//! below 256, so this port works on bytes.

use std::time::{SystemTime, UNIX_EPOCH};

const WINDOW_ENV: &str = "1920|1080|1920|1040|0|30|0|0|1872|92|1920|1040|1857|92|1|24|Win32";
const TABLE_S3: &[u8] = b"ckdp1h4ZKsUB80/Mfvw36XIgR25+WQAlEi7NLboqYTOPuzmFjJnryx9HVGDaStCe";
const TABLE_S4: &[u8] = b"Dkdpgh2ZmsQB80/MfvV36XI1R45-WUAlEixNLwoqYTOPuzKFjJnry79HbGcaStCe";

/// Sign a query string with the current time.
pub fn ab_sign(query: &str, user_agent: &str) -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default();
    ab_sign_at(query, user_agent, now_ms)
}

/// Sign a query string with a fixed start time (milliseconds since the epoch).
pub fn ab_sign_at(query: &str, user_agent: &str, start_time_ms: u64) -> String {
    let mut payload = random_prefix();
    payload.extend(rc4_bb(query, user_agent, start_time_ms));
    let mut signature = result_encrypt(&payload, TABLE_S4);
    signature.push('=');
    signature
}

fn rc4_encrypt(plaintext: &[u8], key: &[u8]) -> Vec<u8> {
    let mut s: Vec<u8> = (0..=255).collect();
    let mut j: usize = 0;
    for i in 0..256 {
        j = (j + s[i] as usize + key[i % key.len()] as usize) % 256;
        s.swap(i, j);
    }
    let mut i: usize = 0;
    j = 0;
    plaintext
        .iter()
        .map(|byte| {
            i = (i + 1) % 256;
            j = (j + s[i] as usize) % 256;
            s.swap(i, j);
            let t = (s[i] as usize + s[j] as usize) % 256;
            s[t] ^ byte
        })
        .collect()
}

/// SM3 hash (GB/T 32905-2016).
pub(crate) fn sm3(data: &[u8]) -> [u8; 32] {
    let mut reg: [u32; 8] = [
        0x7380166f, 0x4914b2b9, 0x172442d7, 0xda8a0600, 0xa96f30bc, 0x163138aa, 0xe38dee4d,
        0xb0fb0e4e,
    ];
    let bit_length = (data.len() as u64) * 8;
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_length.to_be_bytes());

    for block in message.chunks(64) {
        let mut w = [0u32; 68];
        let mut w1 = [0u32; 64];
        for t in 0..16 {
            w[t] = u32::from_be_bytes([
                block[4 * t],
                block[4 * t + 1],
                block[4 * t + 2],
                block[4 * t + 3],
            ]);
        }
        for j in 16..68 {
            let a = w[j - 16] ^ w[j - 9] ^ w[j - 3].rotate_left(15);
            let a = a ^ a.rotate_left(15) ^ a.rotate_left(23);
            w[j] = a ^ w[j - 13].rotate_left(7) ^ w[j - 6];
        }
        for j in 0..64 {
            w1[j] = w[j] ^ w[j + 4];
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = reg;
        for j in 0..64 {
            let t_j: u32 = if j < 16 { 0x79cc4519 } else { 0x7a879d8a };
            let ss1 = a
                .rotate_left(12)
                .wrapping_add(e)
                .wrapping_add(t_j.rotate_left((j % 32) as u32))
                .rotate_left(7);
            let ss2 = ss1 ^ a.rotate_left(12);
            let (ff, gg) = if j < 16 {
                (a ^ b ^ c, e ^ f ^ g)
            } else {
                ((a & b) | (a & c) | (b & c), (e & f) | (!e & g))
            };
            let tt1 = ff.wrapping_add(d).wrapping_add(ss2).wrapping_add(w1[j]);
            let tt2 = gg.wrapping_add(h).wrapping_add(ss1).wrapping_add(w[j]);
            d = c;
            c = b.rotate_left(9);
            b = a;
            a = tt1;
            h = g;
            g = f.rotate_left(19);
            f = e;
            e = tt2 ^ tt2.rotate_left(9) ^ tt2.rotate_left(17);
        }
        for (register, value) in reg.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *register ^= value;
        }
    }

    let mut out = [0u8; 32];
    for (index, value) in reg.iter().enumerate() {
        out[4 * index..4 * index + 4].copy_from_slice(&value.to_be_bytes());
    }
    out
}

/// Custom base64 over 3-byte groups; missing bytes count as zero and the
/// output length is `ceil(len * 4 / 3)`, without padding.
fn result_encrypt(input: &[u8], table: &[u8]) -> String {
    const MASKS: [u32; 4] = [0xfc0000, 0x03f000, 0x000fc0, 0x00003f];
    const SHIFTS: [u32; 4] = [18, 12, 6, 0];
    let total_chars = (input.len() * 4).div_ceil(3);
    let byte_at = |index: usize| input.get(index).copied().unwrap_or(0) as u32;
    (0..total_chars)
        .map(|i| {
            let group = (i / 4) * 3;
            let long_int = (byte_at(group) << 16) | (byte_at(group + 1) << 8) | byte_at(group + 2);
            let index = ((long_int & MASKS[i % 4]) >> SHIFTS[i % 4]) as usize;
            table[index] as char
        })
        .collect()
}

fn gener_random(random_num: u32, option: [u8; 2]) -> [u8; 4] {
    let byte1 = (random_num & 255) as u8;
    let byte2 = ((random_num >> 8) & 255) as u8;
    [
        (byte1 & 170) | (option[0] & 85),
        (byte1 & 85) | (option[0] & 170),
        (byte2 & 170) | (option[1] & 85),
        (byte2 & 85) | (option[1] & 170),
    ]
}

/// StreamGet fixes the "random" values to 0.123456789, 0.987654321 and 0.555555555.
fn random_prefix() -> Vec<u8> {
    let mut bytes = Vec::with_capacity(12);
    bytes.extend(gener_random(1234, [3, 45]));
    bytes.extend(gener_random(9876, [1, 0]));
    bytes.extend(gener_random(5555, [1, 5]));
    bytes
}

fn split_to_bytes(num: u64) -> [u8; 4] {
    [
        (num >> 24) as u8,
        (num >> 16) as u8,
        (num >> 8) as u8,
        num as u8,
    ]
}

fn rc4_bb(query: &str, user_agent: &str, start_time: u64) -> Vec<u8> {
    const SUFFIX: &str = "cus";
    const ARGUMENTS: [u64; 3] = [0, 1, 14];
    const PAGE_ID: u64 = 110624;
    const AID: u64 = 6383;

    let params_hash = sm3(&sm3(format!("{query}{SUFFIX}").as_bytes()));
    let suffix_hash = sm3(&sm3(SUFFIX.as_bytes()));
    let ua_cipher = rc4_encrypt(user_agent.as_bytes(), &[0, 1, 14]);
    let ua_hash = sm3(result_encrypt(&ua_cipher, TABLE_S3).as_bytes());

    let end_time = start_time + 100;
    let mut b = [0u8; 73];
    let flag: u8 = 3;
    b[18] = 44;

    let start = split_to_bytes(start_time);
    b[20..24].copy_from_slice(&start);
    b[24] = (start_time >> 32) as u8;
    b[25] = (start_time >> 40) as u8;

    b[26..30].copy_from_slice(&split_to_bytes(ARGUMENTS[0]));
    b[30] = (ARGUMENTS[1] / 256) as u8;
    b[31] = (ARGUMENTS[1] % 256) as u8;
    let arg1 = split_to_bytes(ARGUMENTS[1]);
    b[32] = arg1[0];
    b[33] = arg1[1];
    b[34..38].copy_from_slice(&split_to_bytes(ARGUMENTS[2]));

    b[38] = params_hash[21];
    b[39] = params_hash[22];
    b[40] = suffix_hash[21];
    b[41] = suffix_hash[22];
    b[42] = ua_hash[23];
    b[43] = ua_hash[24];

    b[44..48].copy_from_slice(&split_to_bytes(end_time));
    b[48] = flag;
    b[49] = (end_time >> 32) as u8;
    b[50] = (end_time >> 40) as u8;

    b[52..56].copy_from_slice(&split_to_bytes(PAGE_ID));
    b[57] = AID as u8;
    b[58] = (AID >> 8) as u8;
    b[59] = (AID >> 16) as u8;
    b[60] = (AID >> 24) as u8;

    let window_env = WINDOW_ENV.as_bytes();
    b[65] = window_env.len() as u8;
    b[66] = (window_env.len() >> 8) as u8;
    b[69] = 0;
    b[70] = 0;
    b[71] = 0;

    b[72] = [
        18, 20, 26, 30, 38, 40, 42, 21, 27, 31, 35, 39, 41, 43, 22, 28, 32, 36, 23, 29, 33, 37, 44,
        45, 46, 47, 48, 49, 50, 24, 25, 52, 53, 54, 55, 57, 58, 59, 60, 65, 66, 70, 71,
    ]
    .iter()
    .fold(0u8, |acc, &index| acc ^ b[index]);

    let order = [
        18, 20, 52, 26, 30, 34, 58, 38, 40, 53, 42, 21, 27, 54, 55, 31, 35, 57, 39, 41, 43, 22, 28,
        32, 60, 36, 23, 29, 33, 37, 44, 45, 59, 46, 47, 48, 49, 50, 24, 25, 65, 66, 70, 71,
    ];
    let mut bb: Vec<u8> = order.iter().map(|&index| b[index]).collect();
    bb.extend_from_slice(window_env);
    bb.push(b[72]);

    rc4_encrypt(&bb, &[121])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn sm3_matches_standard_test_vectors() {
        assert_eq!(
            hex(&sm3(b"abc")),
            "66c7f0f462eeedd9d1f2d46bdc10e4e24167c4875cf2f7a2297da02b8f4ba8e0"
        );
        let long = b"abcd".repeat(16);
        assert_eq!(
            hex(&sm3(&long)),
            "debe9ff92275b8a138604889c18e5a4d6fdb70e5387e5765293dcba39c0c5732"
        );
    }

    #[test]
    fn rc4_matches_known_vector() {
        // RFC 6229-style check: key "Key", plaintext "Plaintext".
        assert_eq!(
            hex(&rc4_encrypt(b"Plaintext", b"Key")),
            "bbf316e8d940af0ad3"
        );
    }

    #[test]
    fn result_encrypt_with_standard_table_is_base64_without_padding() {
        let table = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        assert_eq!(result_encrypt(b"Man", table), "TWFu");
        assert_eq!(result_encrypt(b"Ma", table), "TWE");
        assert_eq!(result_encrypt(b"M", table), "TQ");
    }

    #[test]
    fn ab_sign_is_deterministic_for_a_fixed_time() {
        let ua = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/123.0.0.0 Safari/537.36";
        let first = ab_sign_at("aid=6383&web_rid=1", ua, 1_795_364_602_491);
        let second = ab_sign_at("aid=6383&web_rid=1", ua, 1_795_364_602_491);
        assert_eq!(first, second);
        // 12-byte prefix + 44 + 65 (window env) + 1 bytes => 122 bytes => 163 chars, plus "=".
        assert_eq!(first.len(), 164);
        assert!(first.ends_with('='));
        assert_ne!(
            first,
            ab_sign_at("aid=6383&web_rid=2", ua, 1_795_364_602_491)
        );
    }
}
