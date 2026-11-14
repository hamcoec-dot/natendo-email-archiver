//! Deterministic synthetic mail for tests and benchmarks.

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407) | 1)
    }
    pub fn next(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    pub fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
        &v[(self.next() % v.len() as u64) as usize]
    }
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next() % (hi - lo).max(1)
    }
}

const FIRST: &[&str] = &["Dana", "Bob", "Priya", "Chen", "Maria", "Tom", "Aisha", "Lars", "Yuki", "Omar", "Grace", "Ivan"];
const LAST: &[&str] = &["Smith", "Jones", "Patel", "Wang", "Garcia", "Brown", "Khan", "Berg", "Sato", "Haddad", "Lee", "Novak"];
const DOMAINS: &[&str] = &["acme.com", "example.org", "gmail.com", "contoso.net", "landlord-co.com", "airline.travel", "shop.example"];
const TOPICS: &[&str] = &[
    "invoice", "receipt", "lease renewal", "flight itinerary", "project update", "meeting notes", "heater repair",
    "quarterly report", "dinner plans", "insurance claim", "order confirmation", "password reset", "newsletter",
    "tax documents", "school schedule", "car service", "budget review", "contract draft",
];
const WORDS: &[&str] = &[
    "the", "please", "attached", "thanks", "regarding", "schedule", "payment", "due", "tomorrow", "office",
    "review", "update", "confirm", "details", "account", "delivery", "shipping", "apartment", "maintenance",
    "agreed", "fix", "landlord", "heating", "broken", "window", "contract", "signature", "deadline", "budget",
    "travel", "hotel", "booking", "reference", "number", "customer", "support", "team", "weekend", "family",
];

pub fn person(r: &mut Rng) -> (String, String) {
    let f = *r.pick(FIRST);
    let l = *r.pick(LAST);
    let d = *r.pick(DOMAINS);
    (format!("{f} {l}"), format!("{}.{}@{}", f.to_lowercase(), l.to_lowercase(), d))
}

/// Generate message number `i` (deterministic). Returns raw RFC 5322 bytes.
pub fn message(seed: u64, i: u64) -> Vec<u8> {
    let mut r = Rng::new(seed ^ (i.wrapping_mul(0x9E3779B97F4A7C15)));
    let (fname, femail) = person(&mut r);
    let (tname, temail) = person(&mut r);
    let topic = *r.pick(TOPICS);
    let order = r.range(1000, 99999);
    let ts = 1_262_304_000 + r.range(0, 16 * 365 * 86400) as i64; // 2010..2026
    let date = chrono::DateTime::from_timestamp(ts, 0).unwrap().format("%a, %d %b %Y %H:%M:%S +0000");
    let mut body = String::new();
    let words = r.range(40, 400);
    for w in 0..words {
        body.push_str(r.pick(WORDS));
        body.push(if w % 14 == 13 { '\n' } else { ' ' });
    }
    body.push_str(&format!("\nRe: {topic} #{order}\n"));
    let subject = format!("{}{} #{}", if r.next() % 4 == 0 { "Re: " } else { "" }, capitalize(topic), order);
    let mid = format!("<{seed}.{i}@synth.local>");
    let has_att = r.next() % 5 == 0;
    let mut m = format!(
        "From: {fname} <{femail}>\r\nTo: {tname} <{temail}>\r\nSubject: {subject}\r\nDate: {date}\r\nMessage-ID: {mid}\r\nMIME-Version: 1.0\r\n"
    );
    if has_att {
        let b = format!("b{seed}x{i}");
        let att_text = format!("Attachment about {topic}. Reference {order}. Amount due 42.50 USD.\n");
        m.push_str(&format!("Content-Type: multipart/mixed; boundary=\"{b}\"\r\n\r\n--{b}\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{body}\r\n--{b}\r\nContent-Type: text/plain; name=\"{}-{order}.txt\"\r\nContent-Disposition: attachment; filename=\"{}-{order}.txt\"\r\n\r\n{att_text}\r\n--{b}--\r\n", topic.replace(' ', "_"), topic.replace(' ', "_")));
    } else {
        m.push_str(&format!("Content-Type: text/plain; charset=utf-8\r\n\r\n{body}\r\n"));
    }
    m.into_bytes()
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}
