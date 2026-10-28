//! Synthetic streams: escape-sequence coverage that recorded programs do not
//! reach. `corpus synth` stores them as ordinary entries (`synth-*`); a test
//! keeps the stored streams equal to this generator.

use crate::replay::SYNC_FLUSH_MS;
use crate::store::Meta;
use crate::vtrec::{Event, Header, Kind, Recording, VERSION};

pub struct Synthetic {
    pub name: &'static str,
    pub description: &'static str,
    pub tags: &'static [&'static str],
    pub cols: u16,
    pub rows: u16,
    pub events: Vec<Event>,
}

impl Synthetic {
    pub fn recording(&self) -> Recording {
        Recording {
            header: Header {
                vtrec: VERSION,
                cols: self.cols,
                rows: self.rows,
                program: format!("copilot-canvas corpus synth {}", self.name),
                description: self.description.to_owned(),
                source: "synthetic".into(),
                recorded_at: String::new(),
            },
            events: self.events.clone(),
        }
    }

    pub fn meta(&self) -> Meta {
        Meta {
            description: self.description.to_owned(),
            source: "synthetic".into(),
            cols: self.cols,
            rows: self.rows,
            tags: self.tags.iter().map(|t| (*t).to_owned()).collect(),
            command: vec!["copilot-canvas".into(), "corpus".into(), "synth".into(), self.name.into()],
            ..Meta::default()
        }
    }
}

/// Builds an event list; every chunk is 1 ms after the previous one unless paused.
#[derive(Default)]
struct Stream {
    events: Vec<Event>,
    pause: Option<u64>,
}

impl Stream {
    fn delta(&mut self) -> u64 {
        self.pause.take().unwrap_or(1)
    }

    fn out(&mut self, bytes: impl AsRef<[u8]>) {
        let delta_ms = self.delta();
        self.events.push(Event::output(delta_ms, bytes.as_ref()));
    }

    /// Chunks of `size` bytes: splits escape sequences and UTF-8 sequences.
    fn split(&mut self, bytes: impl AsRef<[u8]>, size: usize) {
        for chunk in bytes.as_ref().chunks(size) {
            self.out(chunk);
        }
    }

    fn mark(&mut self, name: &str) {
        self.events.push(Event { delta_ms: 0, kind: Kind::Mark(name.to_owned()) });
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        let delta_ms = self.delta();
        self.events.push(Event { delta_ms, kind: Kind::Resize(cols, rows) });
    }

    fn pause(&mut self, ms: u64) {
        self.pause = Some(ms);
    }
}

fn unicode() -> Vec<Event> {
    let mut s = Stream::default();
    s.split("Emoji: \u{1F600} \u{1F389} \u{1F44D}\u{1F3FD} \u{2764}\u{FE0F} \u{263A}\u{FE0E} 1\u{FE0F}\u{20E3}\r\n", 5);
    s.split(
        "ZWJ: \u{1F469}\u{200D}\u{1F4BB} \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466} \u{1F3F3}\u{FE0F}\u{200D}\u{1F308} \u{1F9D1}\u{1F3FF}\u{200D}\u{1F680}\r\n",
        3,
    );
    s.out("Flags: \u{1F1EB}\u{1F1F7} \u{1F1EF}\u{1F1F5} \u{1F1FA}\u{1F1F8} lone \u{1F1E6}x\r\n");
    s.out("CJK: 中文字符 日本語 한국어 ｆｕｌｌ\r\n");
    s.split("Marks: e\u{301} a\u{308}\u{332} n\u{303} \u{915}\u{94D}\u{937} \u{1100}\u{1161}\u{11A8}\r\n", 2);
    s.out("ZW: a\u{200B}b c\u{200D}d \u{FEFF}e \u{301}lead\r\n");
    s.out("Ambiguous: \u{2460} \u{B1} \u{BD} \u{2192} \u{2605} \u{25CB}\r\n");
    s.out("Tabs:\t中\t文\tx\r\n");
    s.out(b"Bad UTF-8: \xff \xc3( \xe2\x82 end\r\n");
    // Two columns left: the wide character fits, the next one wraps.
    s.out("\x1b[11;39H中文");
    // One column left: a leading spacer, the wide character wraps.
    s.out("\x1b[13;40H字x");
    // Overwriting half of a wide character erases it.
    s.out("\x1b[15;1H中文字\x1b[15;2Hx");
    s.out("\x1b[16;1H中文字\x1b[16;3Hy");
    // A long line of wide characters wraps at an odd width.
    s.out("\x1b[17;1H\x1b[33m一二三四五六七八九十一二三四五六七八九十一\x1b[0m");
    s.events
}

fn sgr() -> Vec<Event> {
    let mut s = Stream::default();
    let mut head = String::from("16 fg: ");
    for n in (30..=37).chain(90..=97) {
        head += &format!("\x1b[{n}m{n}\x1b[39m ");
    }
    head += "\r\n16 bg: ";
    for n in (40..=47).chain(100..=107) {
        head += &format!("\x1b[{n}m{n}\x1b[49m ");
    }
    head += "\x1b[0m\r\n";
    s.out(head);
    let mut palette = String::new();
    for row in 0..8u32 {
        palette += &format!("256 {:>3}: ", row * 32);
        for i in row * 32..row * 32 + 32 {
            palette += &format!("\x1b[48;5;{i}m ");
        }
        palette += "\x1b[0m ";
        for i in (row * 32..row * 32 + 32).step_by(4) {
            palette += &format!("\x1b[38:5:{i}m#");
        }
        palette += "\x1b[0m\r\n";
    }
    s.split(palette, 64);
    let mut rgb = String::from("rgb fg: ");
    for i in 0..32u32 {
        let v = i * 8;
        rgb += &format!("\x1b[38;2;{v};{};{}m\u{2588}", 255 - v, (v * 3) % 256);
    }
    rgb += "\x1b[0m\r\nrgb bg: ";
    for i in 0..32u32 {
        let v = i * 8;
        rgb += &format!("\x1b[48;2;{};{v};{}m ", 255 - v, (v * 5) % 256);
    }
    rgb += "\x1b[0m\r\n";
    s.split(rgb, 11);
    let rest = concat!(
        "colon: \x1b[38:2::255:128:0mcolorspace\x1b[0m \x1b[38:2:0:128:255mnone\x1b[0m \x1b[48:2::10:20:30mbg\x1b[0m \x1b[38;5;196;48;5;21mmixed\x1b[0m\r\n",
        "attrs: \x1b[1mbold\x1b[22m \x1b[2mdim\x1b[22m \x1b[3mitalic\x1b[23m \x1b[4munder\x1b[24m \x1b[5mblink\x1b[25m \x1b[7minverse\x1b[27m ",
        "\x1b[8mhidden\x1b[28m \x1b[9mstrike\x1b[29m \x1b[53moverline\x1b[55m \x1b[1;2mbold+dim\x1b[0m\r\n",
        "ul: \x1b[4:0mnone\x1b[0m \x1b[4:1msingle\x1b[0m \x1b[4:2mdouble\x1b[0m \x1b[4:3mcurly\x1b[0m \x1b[4:4mdotted\x1b[0m ",
        "\x1b[4:5mdashed\x1b[0m \x1b[21mdbl21\x1b[0m \x1b[4:3m\x1b[24moff\x1b[0m\r\n",
        "ulc: \x1b[4;58;2;255;0;0mred\x1b[0m \x1b[4;58;5;27mblue\x1b[0m \x1b[4;58:2::0:200:0mgreen\x1b[0m ",
        "\x1b[4:3;58;5;208mcurly-orange\x1b[59mdefault\x1b[0m \x1b[58;2;1;2;3mno-underline\x1b[0m\r\n",
        "combo: \x1b[1;31mbold-red\x1b[0m \x1b[2;32mdim-green\x1b[0m \x1b[7;33;44minv-y-b\x1b[0m ",
        "\x1b[1;3;4;9;38;2;200;100;50;48;5;236mall\x1b[0m \x1b[31;41msame\x1b[0m \x1b[1;90mbright-black\x1b[0m\r\n",
        "reset: \x1b[1;4;31;42mx\x1b[mdefault \x1b[1;31mA\x1b[39mB\x1b[22mC \x1b[44mD\x1b[49mE\x1b[0m\r\n",
        "\x1b[44mbce:\x1b[K\x1b[0m\r\n",
        "\x1b[45m\x1b[2Kfull-line-bce\x1b[0m\r\n",
        "\x1b[46mecho-erase\x1b[6D\x1b[3X\x1b[0m\r\n",
        "edge: \x1b[38;5;300mbad256\x1b[0m \x1b[38;2;300;0;0mbadrgb\x1b[0m \x1b[99mnine9\x1b[0m \x1b[;1mempty\x1b[0m \x1b[1;;4mempty2\x1b[0m\r\n",
    );
    s.split(rest, 97);
    s.events
}

fn osc() -> Vec<Event> {
    let mut s = Stream::default();
    s.out("\x1b]0;window title\x07");
    s.out("titles: OSC 0, 1, 2\r\n\x1b]1;icon name\x07\x1b]2;final title\x1b\\");
    s.out("plain \x1b]8;;https://example.com/a\x1b\\link-a\x1b]8;;\x1b\\ text\r\n");
    s.out("id \x1b]8;id=doc;https://example.com/doc\x07part1\x1b]8;;\x07 gap \x1b]8;id=doc;https://example.com/doc\x07part2\x1b]8;;\x07\r\n");
    s.out("adjacent \x1b]8;;https://a.test/1\x1b\\one\x1b]8;;https://a.test/2\x1b\\two\x1b]8;;\x1b\\\r\n");
    s.out("styled \x1b[4;34m\x1b]8;;file:///C:/tmp/x.txt\x1b\\file\x1b]8;;\x1b\\\x1b[0m unicode \x1b]8;;https://example.jp/パス\x1b\\日本\x1b]8;;\x1b\\\r\n");
    s.out("\x1b[50Gwrap \x1b]8;;https://example.com/wrapped\x1b\\wrapped-link-text\x1b]8;;\x1b\\\r\n");
    s.split("split \x1b]8;;https://example.com/split\x1b\\split-link\x1b]8;;\x1b\\\r\n", 4);
    s.out("clipboard \x1b]52;c;aGVsbG8gY2xpcGJvYXJk\x07 query \x1b]52;c;?\x07\r\n");
    s.out("progress \x1b]9;4;1;25\x07");
    s.split("\x1b]9;4;3;0\x1b\\", 3);
    s.out("\x1b]9;4;2;50\x07\x1b]9;4;4;60\x07\x1b]9;4;0;0\x07 states 1 3 2 4 0\r\n");
    s.out("notify \x1b]777;notify;Build;done\x07 osc9 \x1b]9;plain message\x07\r\n");
    s.out("bell \x07\x07 two\r\n");
    s.out("\x1b]10;?\x07\x1b]11;?\x1b\\\x1b]4;1;?\x07\x1b]7;file://host/C:/work\x1b\\\x1b]133;A\x07\x1b]633;P;Cwd=C:/\x07ignored OSCs\r\n");
    s.out("\x1b]4;1;rgb:12/34/56\x07\x1b[31mpalette-red\x1b[0m\x1b]104;1\x07");
    s.events
}

fn screen() -> Vec<Event> {
    let mut s = Stream::default();
    let mut primary = String::new();
    for i in 1..=30 {
        primary += &format!("primary line {i:02}\r\n");
    }
    s.out(primary);
    s.out("\x1b[1;31mprompt>\x1b[0m ");
    s.mark("primary");
    s.out("\x1b7\x1b[?1049h\x1b[2J\x1b[H");
    s.out("\x1b#8\x1b[3;3H\x1b[1J\x1b[12;10H\x1b[0J\x1b[5;5H\x1b[1K\x1b[6;20H\x1b[K\x1b[7;1H\x1b[2K");
    s.mark("decaln-erase");
    s.out("\x1b[2J\x1b[H\x1b(0lqqqqqqqqk\r\nx  dec   x\r\nmqqqqqqqqj\x1b(B \u{250C}\u{2500}\u{2500}\u{2510}\x1b[1B\x1b[4D\u{2514}\u{2500}\u{2500}\u{2518}");
    s.out("\x1b[4;1Hheader (fixed)\x1b[13;1Hfooter (fixed)\x1b[5;12r\x1b[5;1H");
    let mut region = String::new();
    for i in 1..=12 {
        region += &format!("region line {i}\r\n");
    }
    s.out(region);
    s.out("\x1b[5;1H\x1bMreverse-index\x1b[2S\x1b[1T\x1b[7;1H\x1b[2L\x1b[9;1H\x1b[M");
    s.out("\x1b[10;1Hinsert-delete-chars\x1b[10;3H\x1b[3@\x1b[10;12H\x1b[2P\x1b[10;15H\x1b[3X");
    s.mark("scroll-region");
    s.out("\x1b[?6h\x1b[1;1Horigin-home\x1b[?6l\x1b[r");
    s.out("\x1b[2;30H\x1b7saved\x1b[14;1H\x1b8<-restored\x1b[3;30H\x1b[ssco\x1b[1;1H\x1b[u<-sco");
    s.out("\x1b[?7l\x1b[1;40Hno-wrap-overflowing-text\x1b[?7h");
    s.out("\x1b[3g\x1b[1;5H\x1bH\x1b[1;15H\x1bH\x1b[11;1H\tA\tB\x1b[ZC");
    s.out("\x1b[12;1H=\x1b[9b\x1b[12;3H\x1b[4hINS\x1b[4l");
    s.mark("alt-screen");
    s.out("\x1b[?1049lback on primary\r\n");
    s.events
}

fn modes() -> Vec<Event> {
    let mut s = Stream::default();
    s.out("modes, cursor shapes, queries\r\n");
    for (bytes, mark) in [
        ("\x1b[?1000h", "mouse-1000"),
        ("\x1b[?1002h", "mouse-1002"),
        ("\x1b[?1003h", "mouse-1003"),
        ("\x1b[?1006h", "sgr-1006"),
        ("\x1b[?1005h", "utf8-1005"),
        ("\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l", "mouse-off"),
        ("\x1b[?2004h", "bracketed-paste"),
        ("\x1b[?2004l\x1b[?1004h", "focus"),
        ("\x1b[?1004l\x1b[?1h\x1b=", "app-cursor-keypad"),
        ("\x1b[?1l\x1b>\x1b[?25l", "cursor-hidden"),
        ("\x1b[?25h\x1b[>1u", "kitty-push-1"),
        ("\x1b[?u", "kitty-query"),
        ("\x1b[=3;1u", "kitty-set-3"),
        ("\x1b[>8u", "kitty-push-8"),
        ("\x1b[<u", "kitty-pop"),
        ("\x1b[<u", "kitty-pop-again"),
        ("\x1b[<u", "kitty-pop-empty"),
        ("\x1b[0 q", "cursor-0"),
        ("\x1b[1 q", "cursor-1"),
        ("\x1b[2 q", "cursor-2"),
        ("\x1b[3 q", "cursor-3"),
        ("\x1b[4 q", "cursor-4"),
        ("\x1b[5 q", "cursor-5"),
        ("\x1b[6 q", "cursor-6"),
        ("\x1b[2 q\x1b[?12h", "blink-on"),
        ("\x1b[?12l", "blink-off"),
        ("\x1b[c\x1b[>c\x1b[5n\x1b[6n\x1b[?2026$p\x1b[?2004$p\x1b[>q\x1b[14t\x1b[18t", "queries"),
    ] {
        s.out(bytes);
        s.mark(mark);
    }
    s.out("\x1b[?1049h\x1b[?1000h\x1b[?1006h\x1b[?2004h\x1b[>1u\x1b[5 qtui-like modes");
    s.events
}

fn sync() -> Vec<Event> {
    let mut s = Stream::default();
    s.out("before sync\r\n");
    s.mark("before");
    s.out("\x1b[?2026h\x1b[2J\x1b[Hframe 1 line a\r\n");
    s.out("frame 1 line b");
    s.mark("inside-sync");
    s.out("\x1b[?2026l");
    s.mark("after-end");
    s.out("\x1b[?2026h\x1b[2J\x1b[Hunterminated");
    s.pause(SYNC_FLUSH_MS + 50);
    s.out("\r\nlate");
    s.mark("after-timeout");
    s.out("\x1b[?2026h\r\npending at end");
    s.events
}

fn resize() -> Vec<Event> {
    let mut s = Stream::default();
    s.out("short line\r\n");
    s.out("The quick brown fox jumps over the lazy dog; the lazy dog sleeps through it all.\r\n");
    s.out("wide: 中文字符中文字符中文字符中文字符中文字符中\r\n");
    s.out("\x1b[44mcoloured background line\x1b[K\x1b[0m\r\n");
    s.out("prompt> cursor-here");
    s.mark("before");
    s.resize(20, 8);
    s.mark("narrow");
    s.resize(60, 10);
    s.mark("wide");
    s.resize(40, 5);
    s.mark("short");
    s.resize(40, 8);
    s.out(" more");
    s.events
}

fn bytewise() -> Vec<Event> {
    let mut s = Stream::default();
    let text = "\x1b[1;38;2;10;200;30mgreen\x1b[0m \x1b]8;;https://b.test\x1b\\link\x1b]8;;\x1b\\ 中文 e\u{301} \u{1F469}\u{200D}\u{1F4BB}\x1b]0;bytewise\x07\x1b[4 q\r\nline2\x1b[?2004h\x1b]9;4;1;40\x07\x1b]52;c;Ynl0ZXM=\x1b\\";
    s.split(text, 1);
    s.events
}

/// Every synthetic entry.
pub fn all() -> Vec<Synthetic> {
    vec![
        Synthetic {
            name: "synth-unicode",
            description: "Emoji (presentation selectors, skin tones, keycaps), ZWJ sequences (woman technologist, family, rainbow flag), regional-indicator flags (FR, JP, US), CJK and fullwidth, combining marks, Hangul jamo, zero-width characters, ambiguous widths, invalid UTF-8, wide characters at the right margin (wrap and leading spacer) and half-overwritten wide characters; reads split inside UTF-8 sequences",
            tags: &["unicode", "emoji", "cjk", "zwj", "wrap"],
            cols: 40,
            rows: 18,
            events: unicode(),
        },
        Synthetic {
            name: "synth-sgr",
            description: "SGR matrix: 16 colours fg/bg, the 256-colour palette (semicolon and colon forms), truecolor, bold/dim/italic/underline/blink/inverse/hidden/strike/overline and their resets, underline styles 4:0..4:5 and 21, underline colour 58/59, background colour erase, invalid parameters; reads split inside sequences",
            tags: &["sgr", "color", "underline"],
            cols: 100,
            rows: 32,
            events: sgr(),
        },
        Synthetic {
            name: "synth-osc",
            description: "OSC 0/1/2 titles, OSC 8 hyperlinks (ids, BEL/ST, adjacent, styled, wrapped, split reads), OSC 52 clipboard write and refused read, OSC 9;4 progress states, OSC 777 and OSC 9 notifications, BEL, colour queries (OSC 4/10/11) and ignored OSC 7/133/633/104",
            tags: &["osc", "hyperlink", "clipboard", "progress", "notify", "bell", "title"],
            cols: 60,
            rows: 18,
            events: osc(),
        },
        Synthetic {
            name: "synth-screen",
            description: "Scrollback, alternate screen (1049) with cursor save/restore, DECALN and erase variants, DEC special graphics and box drawing, scroll region with LF/RI/SU/SD/IL/DL, ICH/DCH/ECH, origin mode, DECSC/DECRC and SCOSC/SCORC, autowrap off, tab stops (HTS/TBC/CBT), REP and insert mode",
            tags: &["screen", "alt-screen", "scroll-region", "cursor"],
            cols: 50,
            rows: 14,
            events: screen(),
        },
        Synthetic {
            name: "synth-modes",
            description: "Mode changes with a checkpoint after each: mouse 1000/1002/1003/1006/1005, bracketed paste, focus, application cursor/keypad, cursor visibility, kitty keyboard push/query/set/pop (CSI > 1 u), DECSCUSR shapes 0..6, cursor blink, and query replies (DA1, DA2, DSR, DECRQM, XTVERSION, window size)",
            tags: &["modes", "mouse", "kitty-keyboard", "cursor-shape", "queries"],
            cols: 40,
            rows: 6,
            events: modes(),
        },
        Synthetic {
            name: "synth-sync",
            description: "Synchronized updates (DEC 2026): a terminated update, one flushed by the timeout and one still open at the end of the stream",
            tags: &["sync"],
            cols: 30,
            rows: 5,
            events: sync(),
        },
        Synthetic {
            name: "synth-resize",
            description: "Reflow on resize: wrapped plain and wide text, a coloured line and the cursor across narrower, wider and shorter sizes",
            tags: &["resize", "reflow", "wrap"],
            cols: 40,
            rows: 8,
            events: resize(),
        },
        Synthetic {
            name: "synth-bytewise",
            description: "SGR, OSC 8, UTF-8, a combining mark, a ZWJ sequence, a title, DECSCUSR, bracketed paste, progress and OSC 52, fed one byte per read",
            tags: &["chunking", "parser"],
            cols: 40,
            rows: 4,
            events: bytewise(),
        },
    ]
}
