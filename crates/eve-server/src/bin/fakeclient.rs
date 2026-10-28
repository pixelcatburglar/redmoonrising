use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use eve_marshal::{StringTable, Value};
use eve_proto::{
    decode_payload, decompress, encode_payload, llv, FrameReader, MachoAddress, MachoPacket,
    MessageType, ProtoError, VersionChallenge,
};

const IDLE: Duration = Duration::from_millis(1500);

const CALL_TIMEOUT: Duration = Duration::from_secs(30);

const TRAILING: Duration = Duration::from_millis(500);

fn main() -> Result<(), String> {
    let opts = parse_args()?;
    let script = match &opts.script {
        Some(path) => {
            let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
            Some(parse_script(&text).map_err(|e| format!("{path}: {e}"))?)
        }
        None => None,
    };

    let strings_path = std::env::var("RMR_STRINGS").unwrap_or_else(|_| {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../eve-marshal/tests/data/strings.txt").to_string()
    });
    let table = StringTable::parse(
        &std::fs::read(&strings_path).map_err(|e| format!("cannot read {strings_path}: {e}"))?,
    );

    let stream = TcpStream::connect(&opts.addr).map_err(|e| format!("connect {}: {e}", opts.addr))?;
    let _ = stream.set_nodelay(true);
    println!("connected to {}", opts.addr);
    let mut wire = Wire { stream, reader: FrameReader::new(), table, next_call_id: 1, user_id: None };

    let challenge = wire.recv_value(CALL_TIMEOUT)?.ok_or("no LLV challenge from the server")?;
    let server = VersionChallenge::from_value(&challenge).map_err(|e| e.to_string())?;
    println!(
        "server LLV: magic={} machoVersion={} users={} boot={} build={} codename={:?}",
        server.magic,
        server.macho_version,
        server.user_count,
        server.boot_version,
        server.boot_build,
        String::from_utf8_lossy(&server.codename)
    );
    server.validate().map_err(|e| e.to_string())?;
    wire.send_value(&VersionChallenge::client().to_value())?;
    println!("sent client LLV");

    let init_vals = authenticate(&mut wire, &opts.user, &opts.password)?;

    if opts.no_cache {
        println!("\n(skipping {} cached objects: --no-cache)", init_vals.len());
    } else {
        fetch_cached_objects(&mut wire, &init_vals)?;
    }

    println!("\n=== post-login service calls ===");
    fixed_call(&mut wire, "authentication", "Ping", vec![])?;
    fixed_call(&mut wire, "authentication", "GetPostAuthenticationMessage", vec![])?;
    let characters = fixed_call(&mut wire, "character", "GetCharactersToSelect", vec![])?;

    let char_id = script.as_ref().and_then(|s| s.char_id).or_else(|| {
        characters.as_ref().and_then(|v| rowset_column(v, "characterID")).and_then(|c| c.first().copied())
    });
    match char_id {
        Some(id) => {
            println!("\n=== entering game as character {id} ===");
            fixed_call(&mut wire, "character", "SelectCharacterID", vec![Value::Int(id)])?;
        }
        None => println!("\n(no character on this account and no !char -- not entering the game)"),
    }

    let Some(script) = script else { return Ok(()) };
    println!("\n=== {} scripted call(s) ===", script.calls.len());
    let mut failed = 0usize;
    for c in &script.calls {
        if c.pause_before > 0 {
            wire.wait(Duration::from_secs(c.pause_before))?;
        }
        let reply = wire.call(&c.service, &c.method, c.args.clone(), c.kwargs.clone())?;
        let mark = if c.expect_refusal { "?" } else { "" };
        let name = format!("{mark}{}.{}", c.service, c.method);
        match (reply, c.expect_refusal) {
            (Reply::Value(v), false) => {
                println!("  ok   {name} -> {}", describe(&v, &wire.table, 0));
                // FAKECLIENT_FULL=1 prints the whole value too.
                if std::env::var_os("FAKECLIENT_FULL").is_some() {
                    println!("{v:?}");
                }
            }
            (Reply::Refused(why), true) => println!("  ok   {name} -> refused, as expected: {why}"),
            (Reply::Value(v), true) => {
                failed += 1;
                println!(
                    "  FAIL {name} -> {} (line {}) -- answered, but the script expects a refusal; \
                     drop the `?` if that is now correct",
                    describe(&v, &wire.table, 0),
                    c.line
                );
            }
            (Reply::Refused(why), false) => {
                failed += 1;
                println!("  FAIL {name} -> refused: {why} (line {})", c.line);
            }
            (Reply::NoReply, _) => {
                failed += 1;
                println!("  FAIL {name} -> no reply within {}s (line {})", CALL_TIMEOUT.as_secs(), c.line);
            }
            (Reply::Unusable(why), _) => {
                failed += 1;
                println!("  FAIL {name} -> no usable reply: {why} (line {})", c.line);
            }
        }
    }
    if script.final_pause > 0 {
        wire.wait(Duration::from_secs(script.final_pause))?;
    }
    wire.drain(TRAILING)?;

    let total = script.calls.len();
    println!("\n{total} call(s), {} ok, {failed} failed", total - failed);
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

struct Options {
    addr: String,
    user: String,
    password: String,
    no_cache: bool,
    script: Option<String>,
}

fn parse_args() -> Result<Options, String> {
    let mut positional = Vec::new();
    let mut no_cache = false;
    let mut script = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--no-cache" {
            no_cache = true;
        } else if a == "--script" {
            script = Some(args.next().ok_or("--script needs a file")?);
        } else if let Some(path) = a.strip_prefix("--script=") {
            script = Some(path.to_string());
        } else if a.starts_with("--") {
            return Err(format!("unknown flag {a}"));
        } else {
            positional.push(a);
        }
    }
    if positional.len() > 3 {
        return Err(format!("unexpected argument {:?}", positional[3]));
    }
    let mut p = positional.into_iter();
    Ok(Options {
        addr: p.next().unwrap_or_else(|| "127.0.0.1:26000".to_string()),
        user: p.next().unwrap_or_else(|| "testuser".to_string()),
        password: p.next().unwrap_or_else(|| "testpass".to_string()),
        no_cache,
        script,
    })
}

#[derive(Debug)]
struct Script {
    char_id: Option<i64>,
    calls: Vec<ScriptCall>,
    /// Seconds to keep listening after the last call (`!sleep` at the end).
    final_pause: u64,
}

#[derive(Debug)]
struct ScriptCall {
    line: usize,
    service: String,
    method: String,
    args: Vec<Value>,
    kwargs: Vec<(Value, Value)>,
    expect_refusal: bool,
    /// Seconds to listen for notifications before making the call (`!sleep N`).
    pause_before: u64,
}

fn parse_script(text: &str) -> Result<Script, String> {
    let mut script = Script { char_id: None, calls: Vec::new(), final_pause: 0 };
    let mut pause = 0u64;
    for (i, raw) in text.lines().enumerate() {
        let n = i + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("!char") {
            let id = rest.trim().parse().map_err(|_| format!("line {n}: bad !char: {:?}", rest.trim()))?;
            script.char_id = Some(id);
            continue;
        }
        if let Some(rest) = line.strip_prefix("!sleep") {
            pause += rest.trim().parse::<u64>().map_err(|_| format!("line {n}: bad !sleep: {:?}", rest.trim()))?;
            continue;
        }
        if line.starts_with('!') {
            return Err(format!("line {n}: unknown directive {line:?}"));
        }
        let (expect_refusal, line) = match line.strip_prefix('?') {
            Some(rest) => (true, rest),
            None => (false, line),
        };
        let (head, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let (service, method) = head
            .split_once('.')
            .filter(|(s, m)| !s.is_empty() && !m.is_empty())
            .ok_or_else(|| format!("line {n}: expected service.method, got {head:?}"))?;
        let (args, kwargs) = Literal::new(rest).arguments().map_err(|e| format!("line {n}: {e}"))?;
        script.calls.push(ScriptCall {
            line: n,
            service: service.to_string(),
            method: method.to_string(),
            args,
            kwargs,
            expect_refusal,
            pause_before: std::mem::take(&mut pause),
        });
    }
    script.final_pause = pause;
    Ok(script)
}

struct Literal<'a> {
    s: &'a [u8],
    pos: usize,
}

impl<'a> Literal<'a> {
    fn new(s: &'a str) -> Self {
        Literal { s: s.as_bytes(), pos: 0 }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(|c| c.is_ascii_whitespace()) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, c: u8) -> Result<(), String> {
        self.skip_ws();
        if self.peek() == Some(c) {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!("expected '{}' at column {}", c as char, self.pos + 1))
        }
    }

    fn arguments(mut self) -> Result<(Vec<Value>, Vec<(Value, Value)>), String> {
        let (mut args, mut kwargs) = (Vec::new(), Vec::new());
        loop {
            self.skip_ws();
            if self.peek().is_none() {
                return Ok((args, kwargs));
            }
            match self.keyword() {
                Some(name) => kwargs.push((Value::bytes(name), self.value()?)),
                None => args.push(self.value()?),
            }
        }
    }

    fn keyword(&mut self) -> Option<String> {
        let start = self.pos;
        let mut end = start;
        while self.s.get(end).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_') {
            end += 1;
        }
        let is_name = end > start && !self.s[start].is_ascii_digit();
        if is_name && self.s.get(end) == Some(&b'=') {
            self.pos = end + 1;
            return Some(String::from_utf8_lossy(&self.s[start..end]).into_owned());
        }
        None
    }

    fn value(&mut self) -> Result<Value, String> {
        self.skip_ws();
        match self.peek() {
            None => Err("expected a value, found end of line".to_string()),
            Some(b'"') => self.string(),
            Some(b'[') => Ok(Value::List(self.sequence(b'[', b']')?)),
            Some(b'(') => Ok(Value::Tuple(self.sequence(b'(', b')')?)),
            Some(b'{') => self.dict(),
            Some(c) if c.is_ascii_digit() || c == b'-' || c == b'+' => self.number(),
            Some(_) => self.word(),
        }
    }

    fn string(&mut self) -> Result<Value, String> {
        self.pos += 1;
        let mut out = Vec::new();
        loop {
            let Some(c) = self.peek() else { return Err("unterminated string".to_string()) };
            self.pos += 1;
            match c {
                b'"' => return Ok(Value::Bytes(out)),
                b'\\' => {
                    let e = self.peek().ok_or("unterminated escape")?;
                    self.pos += 1;
                    out.push(match e {
                        b'n' => b'\n',
                        b't' => b'\t',
                        b'r' => b'\r',
                        other => other,
                    });
                }
                other => out.push(other),
            }
        }
    }

    fn sequence(&mut self, open: u8, close: u8) -> Result<Vec<Value>, String> {
        self.expect(open)?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(close) {
            self.pos += 1;
            return Ok(items);
        }
        loop {
            items.push(self.value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(c) if c == close => {
                    self.pos += 1;
                    return Ok(items);
                }
                _ => return Err(format!("expected ',' or '{}' at column {}", close as char, self.pos + 1)),
            }
        }
    }

    fn dict(&mut self) -> Result<Value, String> {
        self.expect(b'{')?;
        let mut pairs = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Dict(pairs));
        }
        loop {
            let k = self.value()?;
            self.expect(b':')?;
            pairs.push((k, self.value()?));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Value::Dict(pairs));
                }
                _ => return Err(format!("expected ',' or '}}' at column {}", self.pos + 1)),
            }
        }
    }

    fn token(&mut self) -> &'a str {
        let s: &'a [u8] = self.s;
        let start = self.pos;
        while self.peek().is_some_and(|c| !c.is_ascii_whitespace() && !b",:]})".contains(&c)) {
            self.pos += 1;
        }
        std::str::from_utf8(&s[start..self.pos]).unwrap_or("")
    }

    fn number(&mut self) -> Result<Value, String> {
        let t = self.token();
        if t.contains(['.', 'e', 'E']) {
            t.parse().map(Value::Double).map_err(|_| format!("bad float {t:?}"))
        } else {
            t.parse().map(Value::Int).map_err(|_| format!("bad int {t:?}"))
        }
    }

    fn word(&mut self) -> Result<Value, String> {
        match self.token() {
            "none" | "None" => Ok(Value::None),
            "true" | "True" => Ok(Value::Bool(true)),
            "false" | "False" => Ok(Value::Bool(false)),
            other => Err(format!("unknown literal {other:?} (strings need quotes)")),
        }
    }
}

enum Reply {
    Value(Value),
    Refused(String),
    NoReply,
    Unusable(String),
}

struct Wire {
    stream: TcpStream,
    reader: FrameReader,
    table: StringTable,
    next_call_id: i64,
    user_id: Option<i64>,
}

impl Wire {
    fn send_value(&mut self, v: &Value) -> Result<(), String> {
        let frame = encode_payload(v, &self.table).map_err(|e| e.to_string())?;
        self.stream.write_all(&frame).map_err(|e| format!("write: {e}"))
    }

    fn send(&mut self, p: &MachoPacket) -> Result<(), String> {
        self.send_value(&p.to_value().map_err(|e| e.to_string())?)
    }

    fn recv_value(&mut self, wait: Duration) -> Result<Option<Value>, String> {
        let deadline = Instant::now() + wait;
        loop {
            if let Some(frame) = self.reader.next_frame().map_err(|e| e.to_string())? {
                return match decode_payload(&frame, &self.table) {
                    Ok(v) => Ok(Some(v)),
                    Err(ProtoError::PeerClosed { reason, .. }) => {
                        Err(format!("server closed the connection: {reason}"))
                    }
                    Err(e) => Err(format!("undecodable frame of {} bytes: {e}", frame.len())),
                };
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            self.stream.set_read_timeout(Some(left)).map_err(|e| e.to_string())?;
            let mut buf = [0u8; 65536];
            match self.stream.read(&mut buf) {
                Ok(0) => return Err("server closed the connection".to_string()),
                Ok(n) => self.reader.push(&buf[..n]),
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    return Ok(None)
                }
                Err(e) => return Err(format!("read: {e}")),
            }
        }
    }

    fn recv(&mut self, wait: Duration) -> Result<Option<MachoPacket>, String> {
        match self.recv_value(wait)? {
            Some(v) => MachoPacket::from_value(&v).map(Some).map_err(|e| e.to_string()),
            None => Ok(None),
        }
    }

    /// Listens for `time`, printing whatever arrives.
    fn wait(&mut self, time: Duration) -> Result<(), String> {
        let deadline = Instant::now() + time;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(());
            }
            if let Some(p) = self.recv(left)? {
                show_unsolicited(&p, &self.table);
            }
        }
    }

    fn drain(&mut self, quiet: Duration) -> Result<(), String> {
        while let Some(p) = self.recv(quiet)? {
            show_unsolicited(&p, &self.table);
        }
        Ok(())
    }

    fn call(
        &mut self,
        service: &str,
        method: &str,
        args: Vec<Value>,
        mut kwargs: Vec<(Value, Value)>,
    ) -> Result<Reply, String> {
        let id = self.next_call_id;
        self.next_call_id += 1;
        kwargs.push((Value::bytes("machoVersion"), Value::Int(1)));
        let inner = Value::Tuple(vec![
            Value::Int(1),
            Value::bytes(method),
            Value::Tuple(args),
            Value::Dict(kwargs),
        ]);
        let blob = eve_marshal::save(&inner, &self.table).map_err(|e| e.to_string())?;
        let mut req = MachoPacket::call_req(Value::Tuple(vec![Value::Int(0), Value::MarshalStream(blob)]));
        req.source = Some(MachoAddress::Client { client_id: None, call_id: Some(id), service: None });
        req.destination = Some(MachoAddress::Any { service: Some(service.as_bytes().to_vec()), call_id: None });
        req.user_id = self.user_id;
        self.send(&req)?;

        let deadline = Instant::now() + CALL_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let Some(p) = self.recv(left)? else { return Ok(Reply::NoReply) };
            let ours = p.destination.as_ref().and_then(|d| d.call_id()) == Some(id);
            match p.command {
                MessageType::CallRsp if ours => {
                    return Ok(match p.body.first() {
                        Some(Value::Bytes(b)) | Some(Value::MarshalStream(b)) => {
                            match eve_marshal::load(b, &self.table) {
                                Ok(v) => Reply::Value(v),
                                Err(e) => Reply::Unusable(format!("return value does not unmarshal: {e}")),
                            }
                        }
                        other => Reply::Unusable(format!("CallRsp body is {other:?}")),
                    });
                }
                MessageType::ErrorResponse if ours => {
                    return Ok(Reply::Refused(describe_error(&p, &self.table)))
                }
                _ => show_unsolicited(&p, &self.table),
            }
        }
    }
}

fn authenticate(wire: &mut Wire, user: &str, password: &str) -> Result<Vec<(String, Value)>, String> {
    let clientinfo = Value::Dict(vec![
        (Value::bytes("version.number"), Value::Double(llv::BOOT_VERSION)),
        (Value::bytes("version.build"), Value::Int(llv::BOOT_BUILD)),
        (Value::bytes("version.project"), Value::Bytes(llv::BOOT_CODENAME.to_vec())),
        (Value::bytes("languageID"), Value::bytes("EN")),
    ]);
    let req = MachoPacket::new(
        MessageType::AuthenticationReq,
        vec![clientinfo, Value::bytes(user), Value::bytes(password)],
    );
    wire.send(&req)?;
    println!("sent AuthenticationReq for {user:?}");

    let mut init_vals = None;
    let deadline = Instant::now() + CALL_TIMEOUT;
    while init_vals.is_none() {
        let left = deadline.saturating_duration_since(Instant::now());
        let Some(p) = wire.recv(left)? else { break };
        match p.command {
            MessageType::AuthenticationRsp => init_vals = Some(show_auth_rsp(wire, &p)),
            MessageType::ErrorResponse => {
                println!("\n<- {} (userID {:?})", p.command.name(), p.user_id);
                println!("   {}", describe_error(&p, &wire.table));
                break;
            }
            _ => show_unsolicited(&p, &wire.table),
        }
    }

    wire.drain(IDLE)?;
    println!("\n(no further traffic for {:.1}s -- server idle)", IDLE.as_secs_f64());

    match init_vals {
        Some(Some(v)) => Ok(v),
        _ => Err("the AuthenticationRsp carried no initvals -- login failed".to_string()),
    }
}

fn show_auth_rsp(wire: &mut Wire, p: &MachoPacket) -> Option<Vec<(String, Value)>> {
    let t = &wire.table;
    println!("\n<- {} (userID {:?})", p.command.name(), p.user_id);
    println!("   serverinfo   {}", p.body.first().map(|v| render(v, t)).unwrap_or_default());
    println!("   userID       {}", p.body.get(1).map(|v| describe(v, t, 2)).unwrap_or_default());
    println!("   role         {}", p.body.get(2).map(|v| describe(v, t, 2)).unwrap_or_default());
    println!("   proxyNodeID  {}", p.body.get(4).map(|v| describe(v, t, 2)).unwrap_or_default());
    wire.user_id = p.body.get(1).and_then(|v| v.as_i64());

    let blob = p.body.get(3).and_then(|v| v.as_bytes())?;
    let inner = match eve_marshal::load(blob, t) {
        Ok(v) => v,
        Err(e) => {
            println!("   payload      FAILED to unmarshal: {e}");
            return None;
        }
    };
    let Some(Value::Dict(pairs)) = inner.seq().and_then(|s| s.first()) else {
        println!("   payload      no initvals dict in {}", describe(&inner, t, 0));
        return None;
    };
    let vals: Vec<(String, Value)> = pairs
        .iter()
        .map(|(k, v)| (k.as_bytes().map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default(), v.clone()))
        .collect();
    println!("   payload      inner marshal ok: {} initvals, {} bytes", vals.len(), blob.len());
    let first: Vec<&str> = vals.iter().take(3).map(|(k, _)| k.as_str()).collect();
    println!("     first 3: {first:?}");
    Some(vals)
}

fn fetch_cached_objects(wire: &mut Wire, init_vals: &[(String, Value)]) -> Result<(), String> {
    println!("\n=== fetching {} cached objects ===", init_vals.len());
    let (mut ok, mut bytes) = (0usize, 0usize);
    for (key, handle) in init_vals {
        let state = match handle {
            Value::Instance { state, .. } => state.seq().unwrap_or_default(),
            _ => &[],
        };
        if state.len() < 3 {
            println!("  {key}: handle is not util.CachedObject: {}", describe(handle, &wire.table, 0));
            continue;
        }
        let args = vec![Value::Int(1), state[0].clone(), state[2].clone(), state[1].clone()];
        let payload = match wire.call("objectCaching", "GetCachableObject", args, Vec::new())? {
            Reply::Value(v) => v,
            Reply::Refused(why) => {
                println!("  {key}: refused: {why}");
                continue;
            }
            Reply::NoReply => {
                println!("  {key}: no reply");
                continue;
            }
            Reply::Unusable(why) => {
                println!("  {key}: {why}");
                continue;
            }
        };
        let pickle = match &payload {
            Value::Instance { state, .. } => state.seq().and_then(|s| s.get(4)).and_then(|v| v.as_bytes()),
            _ => None,
        };
        let Some(pickle) = pickle else {
            println!("  {key}: not an objectCaching.CachedObject: {}", describe(&payload, &wire.table, 0));
            continue;
        };
        match decompress(pickle).map_err(|e| e.to_string()).and_then(|raw| {
            eve_marshal::load(&raw, &wire.table).map(|_| raw.len()).map_err(|e| e.to_string())
        }) {
            Ok(n) => {
                ok += 1;
                bytes += n;
            }
            Err(e) => println!("  {key}: inner payload does not decode: {e}"),
        }
    }
    println!(
        "cached objects: {ok}/{} decoded, {:.1} MB of inner payload",
        init_vals.len(),
        bytes as f64 / 1e6
    );
    Ok(())
}

fn fixed_call(wire: &mut Wire, service: &str, method: &str, args: Vec<Value>) -> Result<Option<Value>, String> {
    let shown: Vec<String> = args.iter().map(|a| describe_arg(a, &wire.table)).collect();
    let head = format!("{service}.{method}({})", shown.join(", "));
    Ok(match wire.call(service, method, args, Vec::new())? {
        Reply::Value(v) => {
            println!("{head} -> {}", describe(&v, &wire.table, 0));
            Some(v)
        }
        Reply::Refused(why) => {
            println!("{head} -> refused: {why}");
            None
        }
        Reply::NoReply => {
            println!("{head} -> no reply");
            None
        }
        Reply::Unusable(why) => {
            println!("{head} -> no usable reply: {why}");
            None
        }
    })
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn describe(v: &Value, table: &StringTable, depth: usize) -> String {
    match v {
        Value::None => "None".to_string(),
        Value::Bool(b) => (if *b { "True" } else { "False" }).to_string(),
        Value::Int(i) => format!("Int({i})"),
        Value::Long(i) => format!("Long({i})"),
        Value::Double(d) => format!("Double({d:?})"),
        Value::Bytes(b) | Value::Buffer(b) => {
            if b.len() > 80 {
                format!("{:?}... ({} bytes)", text(&b[..80]), b.len())
            } else {
                format!("{:?}", text(b))
            }
        }
        Value::Unicode(s) => format!("u{s:?}"),
        Value::Tuple(items) if depth < 2 && items.len() <= 8 => {
            let inner: Vec<String> = items.iter().map(|i| describe(i, table, depth + 1)).collect();
            format!("tuple({}) [{}]", items.len(), inner.join(" | "))
        }
        Value::Tuple(items) => format!("tuple({})", items.len()),
        Value::List(items) => format!("list({})", items.len()),
        Value::Dict(pairs) => format!("dict({})", pairs.len()),
        Value::Instance { class, state } => describe_instance(class, state),
        Value::CallbackObject(inner) => describe_objref(inner, table),
        Value::MarshalStream(b) => format!("MarshalStream({} bytes)", b.len()),
        other => other.type_name().to_string(),
    }
}

fn describe_arg(v: &Value, table: &StringTable) -> String {
    match v {
        Value::Int(i) | Value::Long(i) => i.to_string(),
        other => describe(other, table, 1),
    }
}

fn describe_instance(class: &Value, state: &Value) -> String {
    let name = class.as_bytes().map(text).unwrap_or_else(|| "?".to_string());
    let Value::Dict(pairs) = state else {
        return format!("{name} state={}", state.type_name());
    };
    let Some(header) = state.get_str("header").and_then(|h| h.seq()) else {
        let keys: Vec<String> = pairs.iter().filter_map(|(k, _)| k.as_bytes().map(text)).collect();
        return format!("{name}{{{}}}", keys.join(","));
    };
    let cols: Vec<String> = header.iter().map(|c| c.as_bytes().map(text).unwrap_or_else(|| "?".into())).collect();
    let rows = ["lines", "line", "items"].into_iter().find_map(|k| state.get_str(k).map(|v| (k, v)));
    match rows {
        Some((key, Value::List(l))) | Some((key, Value::Tuple(l))) if key != "line" => {
            format!("{name}{{{key}}} x{} [{}]", l.len(), cols.join(","))
        }
        Some((key, Value::Dict(d))) => format!("{name}{{{key}}} x{} [{}]", d.len(), cols.join(",")),
        Some((key, _)) => format!("{name}{{{key}}} [{}]", cols.join(",")),
        None => format!("{name}{{no rows key}} [{}]", cols.join(",")),
    }
}

fn describe_objref(inner: &Value, table: &StringTable) -> String {
    let decoded = match inner.as_bytes() {
        Some(b) => eve_marshal::load(b, table).ok(),
        None => Some(inner.clone()),
    };
    match decoded.as_ref().and_then(|d| d.seq()).and_then(|s| s.first()).and_then(|v| v.as_bytes()) {
        Some(id) => format!("objref({:?})", text(id)),
        None => format!("objref({})", decoded.map(|d| describe(&d, table, 1)).unwrap_or_else(|| "?".into())),
    }
}

fn render(v: &Value, table: &StringTable) -> String {
    match v {
        Value::Dict(pairs) => {
            let items: Vec<String> = pairs
                .iter()
                .map(|(k, v)| format!("{}: {}", k.as_bytes().map(text).unwrap_or_default(), describe(v, table, 2)))
                .collect();
            format!("{{{}}}", items.join(", "))
        }
        other => describe(other, table, 0),
    }
}

fn describe_error(p: &MachoPacket, table: &StringTable) -> String {
    let code = p.body.get(1).and_then(|v| v.as_i64()).unwrap_or(-1);
    let blob = p.body.get(2).and_then(|v| v.seq()).and_then(|s| s.first()).and_then(|v| v.as_bytes());
    let Some(exc) = blob.and_then(|b| eve_marshal::load(b, table).ok()) else {
        return format!("ErrorResponse code {code}: {:?}", p.body.get(2));
    };
    let Value::Instance { class, state } = &exc else {
        return format!("ErrorResponse code {code}: {}", describe(&exc, table, 0));
    };
    let class = class.as_bytes().map(text).unwrap_or_default();
    let short = class.rsplit('.').next().unwrap_or(&class).to_string();
    let (msg, dict) = match &**state {
        Value::Dict(_) => (
            state.get_str("msg").or_else(|| state.get_str("serviceName")),
            state.get_str("dict"),
        ),
        other => {
            let parts = other.seq().unwrap_or_default();
            (parts.first(), parts.get(1))
        }
    };
    let msg = msg.and_then(|m| m.as_bytes()).map(text).unwrap_or_default();
    let args: Vec<String> = match dict {
        Some(Value::Dict(d)) => d
            .iter()
            .map(|(k, v)| {
                let shown = match v.as_bytes() {
                    Some(b) => format!("{:?}", text(b)),
                    None => describe(v, table, 2),
                };
                format!("{}={shown}", k.as_bytes().map(text).unwrap_or_default())
            })
            .collect(),
        _ => Vec::new(),
    };
    if args.is_empty() {
        format!("{short} {msg}")
    } else {
        format!("{short} {msg}: {}", args.join(", "))
    }
}

fn show_unsolicited(p: &MachoPacket, table: &StringTable) {
    println!("\n<- {} (userID {:?})", p.command.name(), p.user_id);
    match p.command {
        MessageType::SessionChangeNotification => {
            let change = p.body.first().and_then(|b| b.seq()).and_then(|s| s.get(1));
            let Some(Value::Dict(attrs)) = change else {
                println!("   unrecognised body: {}", describe(&Value::Tuple(p.body.clone()), table, 0));
                return;
            };
            println!("   session change, {} attribute(s):", attrs.len());
            for (name, pair) in attrs {
                let name = name.as_bytes().map(text).unwrap_or_default();
                let (old, new) = match pair.seq() {
                    Some([o, n]) => (describe(o, table, 2), describe(n, table, 2)),
                    _ => ("?".to_string(), describe(pair, table, 2)),
                };
                println!("     {name:<16} {old} -> {new}");
            }
        }
        MessageType::Notification => {
            if let Some(MachoAddress::Broadcast { broadcast_id, .. }) = &p.destination {
                println!("   broadcast    {}", describe(broadcast_id, table, 2));
            }
            match unnest_notification(p, table) {
                Ok(args) => println!("   args         {}", describe(&args, table, 0)),
                Err(why) => println!("   payload      FAILED to un-nest: {why}"),
            }
        }
        MessageType::ErrorResponse => println!("   {}", describe_error(p, table)),
        _ => println!("   body {}", describe(&Value::Tuple(p.body.clone()), table, 0)),
    }
}

fn unnest_notification(p: &MachoPacket, table: &StringTable) -> Result<Value, String> {
    let outer = p.body.first().and_then(|v| v.seq()).ok_or("payload is not a tuple")?;
    let blob = match outer.get(1) {
        Some(Value::Bytes(b)) | Some(Value::MarshalStream(b)) => b,
        _ => return Err("payload[1] is not a marshal blob".to_string()),
    };
    let middle = eve_marshal::load(blob, table).map_err(|e| e.to_string())?;
    let inner = middle.seq().and_then(|s| s.get(1)).ok_or("inner layer is not (0, ...)")?;
    let args = inner.seq().and_then(|s| s.get(1)).ok_or("innermost layer is not (1, args)")?;
    Ok(args.clone())
}

fn rowset_column(v: &Value, column: &str) -> Option<Vec<i64>> {
    let Value::Instance { state, .. } = v else { return None };
    let header = state.get_str("header")?.seq()?;
    let idx = header.iter().position(|c| c.as_bytes() == Some(column.as_bytes()))?;
    let lines = state.get_str("lines")?.seq()?;
    Some(lines.iter().filter_map(|l| l.seq()?.get(idx)?.as_i64()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_every_literal_form_the_scripts_use() {
        let (args, kwargs) =
            Literal::new(r#"[60000004,15] none ["CharGetInfo",[],{}] -1234567890 100.0 "a \"b\"\n" (1,2) {"k":true}"#)
                .arguments()
                .unwrap();
        assert_eq!(
            args,
            vec![
                Value::List(vec![Value::Int(60000004), Value::Int(15)]),
                Value::None,
                Value::List(vec![Value::bytes("CharGetInfo"), Value::List(vec![]), Value::Dict(vec![])]),
                Value::Int(-1234567890),
                Value::Double(100.0),
                Value::bytes("a \"b\"\n"),
                Value::Tuple(vec![Value::Int(1), Value::Int(2)]),
                Value::Dict(vec![(Value::bytes("k"), Value::Bool(true))]),
            ]
        );
        assert!(kwargs.is_empty());
    }

    #[test]
    fn name_equals_is_a_kwarg_but_a_quoted_equals_is_not() {
        let (args, kwargs) = Literal::new(r#""item" 40314792 minRange=15000 "a=b""#).arguments().unwrap();
        assert_eq!(args, vec![Value::bytes("item"), Value::Int(40314792), Value::bytes("a=b")]);
        assert_eq!(kwargs, vec![(Value::bytes("minRange"), Value::Int(15000))]);
    }

    #[test]
    fn script_lines_directives_and_refusals() {
        let s = parse_script("# c\n!char 140000001\n\nfoo.Bar 1\n?baz.Qux\n").unwrap();
        assert_eq!(s.char_id, Some(140000001));
        assert_eq!(s.calls.len(), 2);
        assert_eq!((s.calls[0].line, s.calls[0].expect_refusal), (4, false));
        assert_eq!((s.calls[1].service.as_str(), s.calls[1].method.as_str()), ("baz", "Qux"));
        assert!(s.calls[1].expect_refusal);
    }

    #[test]
    fn malformed_scripts_name_the_line() {
        assert!(parse_script("nodot").unwrap_err().starts_with("line 1: expected service.method"));
        assert!(parse_script("\n!char abc").unwrap_err().starts_with("line 2: bad !char"));
        assert!(parse_script("a.B bareword").unwrap_err().contains("strings need quotes"));
    }
}
