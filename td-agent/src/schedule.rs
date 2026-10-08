//! Schedules (DESIGN.md §3): when one fires, a five-field cron expression
//! or a single local time, and the instants either gives in a zone, a
//! local time that does not exist skipped and one that repeats taken at
//! its first instance; and the schedules the window process keeps in the
//! state directory, each with its journal, and which are due.

use td_civil::{Civil, Zone};

/// How far ahead the next firing of a cron expression is looked for: the
/// Gregorian calendar's whole cycle, 400 years and a whole number of
/// weeks, so a date the expression allows is found if one ever comes and
/// none found means none ever will.
const HORIZON_DAYS: i64 = 146_097 + 1;

/// The last instant a firing is looked for after, the end of 9999 UTC:
/// past it td-civil's dates are clamped, and nothing would ever be later.
const LAST_AFTER: i64 = 253_402_300_799;

/// A five-field cron expression: minute, hour, day of month, month, day
/// of week, each the set of values it allows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cron {
    minutes: u64,
    hours: u32,
    days: u32,
    months: u16,
    weekdays: u8,
    /// Whether each day field restricts: one that begins with `*` does
    /// not, as cron reads it.
    days_restricted: bool,
    weekdays_restricted: bool,
}

/// When a schedule fires.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum When {
    Cron(Cron),
    /// Once, at this local time.
    At(Civil),
}

/// One field's allowed values as a mask of bits `least..=most`, from
/// numbers, `*`, lists, ranges and steps, no names or macros.
fn field(text: &str, name: &str, least: u32, most: u32) -> Result<u64, String> {
    let wrong = |why: &str| format!("the {name} field {text:?} {why}");
    let number = |part: &str| -> Result<u32, String> {
        if part.is_empty() || part.len() > 2 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(wrong("has something that is not a number"));
        }
        let n: u32 = part
            .parse()
            .map_err(|_| wrong("has something that is not a number"))?;
        if !(least..=most).contains(&n) {
            return Err(wrong(&format!("has {n}, outside {least} to {most}")));
        }
        Ok(n)
    };
    if text.is_empty() {
        return Err(wrong("is empty"));
    }
    let mut mask = 0u64;
    for item in text.split(',') {
        let (range, step) = match item.split_once('/') {
            Some((range, step)) => {
                // A step is an increment, not a value: `*/60` is minute 0.
                let step = Some(step)
                    .filter(|step| {
                        (1..=3).contains(&step.len()) && step.bytes().all(|b| b.is_ascii_digit())
                    })
                    .and_then(|step| step.parse::<u32>().ok())
                    .filter(|step| *step >= 1)
                    .ok_or_else(|| wrong("has a step that is not a number from 1"))?;
                (range, step)
            }
            None => (item, 1),
        };
        let (from, to) = if range == "*" {
            (least, most)
        } else if let Some((from, to)) = range.split_once('-') {
            let (from, to) = (number(from)?, number(to)?);
            if from > to {
                return Err(wrong("has a range that runs backwards"));
            }
            (from, to)
        } else {
            let from = number(range)?;
            // `N/S` steps from N to the field's end, as cron reads it.
            (from, if item.contains('/') { most } else { from })
        };
        let mut at = from;
        while at <= to {
            mask |= 1u64 << at;
            at = at.saturating_add(step);
        }
    }
    Ok(mask)
}

impl Cron {
    pub fn parse(text: &str) -> Result<Self, String> {
        let fields: Vec<&str> = text.split_whitespace().collect();
        let [minute, hour, day, month, weekday] = fields.as_slice() else {
            return Err(format!(
                "a cron expression has five fields, minute hour day month weekday; {text:?} has {}",
                fields.len()
            ));
        };
        let weekdays = field(weekday, "day of week", 0, 7)?;
        // Both 0 and 7 are Sunday.
        let weekdays = (weekdays | (weekdays >> 7)) & 0x7f;
        Ok(Self {
            minutes: field(minute, "minute", 0, 59)?,
            hours: u32::try_from(field(hour, "hour", 0, 23)?).map_err(|e| e.to_string())?,
            days: u32::try_from(field(day, "day of month", 1, 31)?).map_err(|e| e.to_string())?,
            months: u16::try_from(field(month, "month", 1, 12)?).map_err(|e| e.to_string())?,
            weekdays: u8::try_from(weekdays).map_err(|e| e.to_string())?,
            days_restricted: !day.starts_with('*'),
            weekdays_restricted: !weekday.starts_with('*'),
        })
    }

    /// Whether the date is one this fires on: when both day fields
    /// restrict, either matching suffices, as in cron.
    fn on(&self, year: i64, month: u8, day: u8) -> bool {
        let days = td_civil::tzif::days_from_civil(year, month, day);
        // 1970-01-01 was a Thursday; Sunday is 0.
        let weekday = (days + 4).rem_euclid(7);
        let by_day = self.days & (1u32 << day) != 0;
        let by_weekday = self.weekdays & (1u8 << weekday) != 0;
        let by_month = self.months & (1u16 << month) != 0;
        by_month
            && if self.days_restricted && self.weekdays_restricted {
                by_day || by_weekday
            } else {
                by_day && by_weekday
            }
    }
}

impl When {
    /// A cron expression, or a single local time `YYYY-MM-DDTHH:MM`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        if text.split_whitespace().count() == 5 {
            return Cron::parse(text).map(Self::Cron);
        }
        let civil = td_civil::parse_ymd_hm(text)
            .filter(|_| text.len() == 16 && text.as_bytes().get(10) == Some(&b'T'))
            .ok_or_else(|| {
                format!("{text:?} is neither a five-field cron expression nor a local time YYYY-MM-DDTHH:MM")
            })?;
        Ok(Self::At(civil))
    }

    /// The first instant after `after` this fires at in `zone`, if any:
    /// a local time in a gap is skipped, one that repeats fires at its
    /// first instance.
    pub fn next_after(&self, zone: &Zone, after: i64) -> Option<i64> {
        if after >= LAST_AFTER {
            return None;
        }
        match self {
            Self::At(civil) => zone.from_local_earliest(civil).filter(|at| *at > after),
            Self::Cron(cron) => {
                let (start, _) = zone.to_local(after);
                let first =
                    td_civil::tzif::days_from_civil(i64::from(start.year), start.month, start.day);
                let first = i64::try_from(first).ok()?;
                for day in first..first.saturating_add(HORIZON_DAYS) {
                    let (year, month, date) = td_civil::tzif::civil_from_days(day);
                    if !cron.on(year, month, date) {
                        continue;
                    }
                    let year = i32::try_from(year).ok()?;
                    for hour in (0..24u8).filter(|h| cron.hours & (1u32 << h) != 0) {
                        for minute in (0..60u8).filter(|m| cron.minutes & (1u64 << m) != 0) {
                            let local = Civil::new(year, month, date, hour, minute, 0);
                            match zone.from_local_earliest(&local) {
                                Some(at) if at > after => return Some(at),
                                _ => {}
                            }
                        }
                    }
                }
                None
            }
        }
    }

    /// The next `count` instants after `after`, fewer when it stops.
    pub fn upcoming(&self, zone: &Zone, after: i64, count: usize) -> Vec<i64> {
        let mut found = Vec::with_capacity(count);
        let mut from = after;
        while found.len() < count {
            let Some(at) = self.next_after(zone, from) else {
                break;
            };
            found.push(at);
            from = at;
        }
        found
    }
}

/// The schedules the window keeps (DESIGN.md §3): the file, the most it
/// holds, and the most bytes it may take, each schedule at its longest
/// text escaped.
const SCHEDULES: &str = "schedules";
pub const MAX_SCHEDULES: usize = 64;
const MAX_SCHEDULES_BYTES: u64 = (MAX_SCHEDULES * (6 * crate::tools::MAX_MESSAGE + 1024)) as u64;

/// A schedule: when it fires, the conversation it fires to, the text it
/// delivers, and whether a firing missed while td-agent was not running
/// is made up once at startup. `through` is its journal: every instant up
/// to it is handled, fired or dropped, so none fires twice.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Schedule {
    pub id: String,
    /// As written, which `when` is parsed from.
    pub written: String,
    pub when: When,
    pub to: crate::store::Id,
    pub text: String,
    pub catch_up: bool,
    /// The conversation whose model asked for it, which the person
    /// approved; none when the person made it.
    pub author: Option<crate::store::Id>,
    pub created: u64,
    pub through: u64,
}

/// A firing due: its schedule, and why it starts no turn, when it does
/// not.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Due {
    pub schedule: String,
    pub to: crate::store::Id,
    pub text: String,
    pub author: Option<crate::store::Id>,
    pub skipped: Option<String>,
}

/// The schedules, as the state directory keeps them.
pub struct Schedules {
    root: std::path::PathBuf,
    list: Vec<Schedule>,
    zone: Zone,
    /// The first `due` is at startup: a firing missed before it is
    /// dropped unless its schedule catches up.
    started: bool,
}

impl Schedule {
    fn json(&self) -> td_json::Json {
        use td_json::Json;
        Json::Obj(vec![
            ("id".into(), Json::Str(self.id.clone())),
            ("when".into(), Json::Str(self.written.clone())),
            ("to".into(), Json::Str(self.to.to_string())),
            ("text".into(), Json::Str(self.text.clone())),
            ("catch_up".into(), Json::Bool(self.catch_up)),
            (
                "author".into(),
                self.author
                    .as_ref()
                    .map_or(Json::Null, |a| Json::Str(a.to_string())),
            ),
            ("created".into(), Json::from(self.created)),
            ("through".into(), Json::from(self.through)),
        ])
    }

    fn from_json(value: &td_json::Json) -> Result<Self, String> {
        let string = |key: &str| {
            value
                .get(key)
                .and_then(td_json::Json::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("a schedule with no {key}"))
        };
        let number = |key: &str| {
            value
                .get(key)
                .and_then(td_json::Json::as_u64)
                .ok_or_else(|| format!("a schedule with no {key}"))
        };
        let id = string("id")?;
        if !id_ok(&id) {
            return Err("a schedule's id is not eight hex digits".into());
        }
        let written = string("when")?;
        let text = string("text")?;
        if text.is_empty() || text.len() > crate::tools::MAX_MESSAGE {
            return Err(format!("schedule {id}'s text is empty or too long"));
        }
        Ok(Self {
            when: When::parse(&written)?,
            written,
            to: crate::store::Id::parse(&string("to")?).ok_or("a schedule's to is not an id")?,
            text,
            catch_up: value
                .get("catch_up")
                .and_then(td_json::Json::as_bool)
                .ok_or("a schedule with no catch_up")?,
            author: match value.get("author") {
                Some(td_json::Json::Null) => None,
                Some(td_json::Json::Str(a)) => {
                    Some(crate::store::Id::parse(a).ok_or("a schedule's author is not an id")?)
                }
                _ => return Err("a schedule with no author".into()),
            },
            created: number("created")?,
            through: number("through")?,
            id,
        })
    }

    /// When it fires next after `after`, in `zone`.
    pub fn next(&self, zone: &Zone, after: u64) -> Option<u64> {
        let after = i64::try_from(after).ok()?;
        self.when
            .next_after(zone, after)
            .and_then(|at| u64::try_from(at).ok())
    }
}

/// A schedule's id: eight lowercase hex digits.
fn id_ok(id: &str) -> bool {
    id.len() == 8 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl Schedules {
    /// The schedules `state` keeps, in `zone`; none when there is no file.
    /// A file that cannot be read is set aside, and why is said, so a
    /// schedule made next does not lose the rest silently.
    pub fn load(state: &crate::store::StateDir, zone: Zone) -> (Self, Option<String>) {
        let root = state.root().to_path_buf();
        let path = root.join(SCHEDULES);
        let read = match crate::store::read_bounded(&path, MAX_SCHEDULES_BYTES) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.to_string()),
            Ok(bytes) => std::str::from_utf8(&bytes)
                .map_err(|_| "not UTF-8".to_string())
                .and_then(|text| td_json::parse(text).map_err(|e| e.to_string()))
                .and_then(|value| {
                    let items = value.as_arr().ok_or("not a list")?;
                    if items.len() > MAX_SCHEDULES {
                        return Err(format!("more than {MAX_SCHEDULES} schedules"));
                    }
                    items.iter().map(Schedule::from_json).collect()
                }),
        };
        let (list, problem) = match read {
            Ok(list) => (list, None),
            Err(why) => {
                let aside = root.join(format!("{SCHEDULES}.set-aside-{}", crate::store::now()));
                let moved = std::fs::rename(&path, &aside)
                    .map(|()| format!("it was moved to {}", aside.display()))
                    .unwrap_or_else(|e| format!("it could not be moved aside: {e}"));
                (
                    Vec::new(),
                    Some(format!(
                        "{} could not be read ({why}); {moved}",
                        path.display()
                    )),
                )
            }
        };
        (
            Self {
                root,
                list,
                zone,
                started: false,
            },
            problem,
        )
    }

    pub fn list(&self) -> &[Schedule] {
        &self.list
    }

    /// Writes `list` whole, and keeps it once written.
    fn save(&mut self, list: Vec<Schedule>) -> Result<(), String> {
        let text = format!(
            "{}\n",
            td_json::Json::Arr(list.iter().map(Schedule::json).collect())
        );
        if text.len() as u64 > MAX_SCHEDULES_BYTES {
            return Err(format!(
                "the schedules are past {MAX_SCHEDULES_BYTES} bytes"
            ));
        }
        crate::store::replace(&self.root, SCHEDULES, text.as_bytes())?;
        self.list = list;
        Ok(())
    }

    /// Adds a schedule made at `now`, written whole before this returns:
    /// one that would never fire is refused.
    pub fn add(
        &mut self,
        written: &str,
        to: crate::store::Id,
        text: String,
        catch_up: bool,
        author: Option<crate::store::Id>,
        now: u64,
    ) -> Result<Schedule, String> {
        if self.list.len() >= MAX_SCHEDULES {
            return Err(format!(
                "there are {MAX_SCHEDULES} schedules already; remove one first"
            ));
        }
        if text.trim().is_empty() {
            return Err("a schedule needs the text it delivers".into());
        }
        if text.len() > crate::tools::MAX_MESSAGE {
            return Err(format!(
                "a schedule's text is at most {} bytes",
                crate::tools::MAX_MESSAGE
            ));
        }
        let when = When::parse(written)?;
        let schedule = Schedule {
            id: crate::store::random_hex(4)?,
            written: written.trim().to_string(),
            when,
            to,
            text,
            catch_up,
            author,
            created: now,
            through: now,
        };
        if schedule.next(&self.zone, now).is_none() {
            return Err(format!("{} never comes after now", schedule.written));
        }
        let mut list = self.list.clone();
        list.push(schedule.clone());
        self.save(list)?;
        Ok(schedule)
    }

    /// Removes schedule `id`, by its id or the start of it.
    pub fn remove(&mut self, id: &str) -> Result<Schedule, String> {
        let matching: Vec<usize> = (0..self.list.len())
            .filter(|at| {
                self.list
                    .get(*at)
                    .is_some_and(|s| !id.is_empty() && s.id.starts_with(id))
            })
            .collect();
        let [at] = matching.as_slice() else {
            return Err(match matching.len() {
                0 => format!("no schedule is {id:?}"),
                _ => format!("{id:?} names more than one schedule"),
            });
        };
        let removed = self.list.get(*at).cloned().ok_or("no such schedule")?;
        let list = self
            .list
            .iter()
            .filter(|s| s.id != removed.id)
            .cloned()
            .collect();
        self.save(list)?;
        Ok(removed)
    }

    /// The firings due at `now`, each journaled before this returns, so
    /// none is delivered twice whatever happens after. A schedule fires
    /// once however many of its times have passed since it last did,
    /// except at startup, where a time missed while td-agent was not
    /// running is dropped unless the schedule catches up. `target` says
    /// of a conversation why a firing to it would start no turn, or that
    /// it is archived or gone, which fires nothing: such a schedule's
    /// times pass unfired. A one-off schedule handled is removed.
    pub fn due(
        &mut self,
        now: u64,
        target: &dyn Fn(&crate::store::Id) -> Target,
    ) -> Result<Vec<Due>, String> {
        let startup = !self.started;
        let mut due = Vec::new();
        let mut list = Vec::with_capacity(self.list.len());
        let mut changed = false;
        for schedule in &self.list {
            let Some(next) = schedule.next(&self.zone, schedule.through) else {
                list.push(schedule.clone());
                continue;
            };
            if next > now {
                list.push(schedule.clone());
                continue;
            }
            changed = true;
            let fires = !startup || schedule.catch_up;
            match target(&schedule.to) {
                Target::Live(skipped) if fires => due.push(Due {
                    schedule: schedule.id.clone(),
                    to: schedule.to.clone(),
                    text: schedule.text.clone(),
                    author: schedule.author.clone(),
                    skipped,
                }),
                _ => {}
            }
            if matches!(schedule.when, When::Cron(_)) {
                list.push(Schedule {
                    through: now,
                    ..schedule.clone()
                });
            }
        }
        if changed {
            self.save(list)?;
        }
        // Startup ends once its journal is written: a retry after a
        // failed write still drops what was missed.
        self.started = true;
        Ok(due)
    }

    /// When the soonest schedule fires next.
    pub fn soonest(&self) -> Option<u64> {
        self.list
            .iter()
            .filter_map(|s| s.next(&self.zone, s.through))
            .min()
    }

    /// A UTC instant as a local time, `YYYY-MM-DD HH:MM`.
    pub fn local(&self, at: u64) -> String {
        let (civil, _) = self.zone.to_local(i64::try_from(at).unwrap_or(i64::MAX));
        format!(
            "{} {:02}:{:02}",
            td_civil::format_ymd(&civil),
            civil.hour,
            civil.minute
        )
    }

    /// Schedule `s`'s next `count` times after `now`, as local times.
    pub fn upcoming(&self, s: &Schedule, now: u64, count: usize) -> Vec<String> {
        let after = i64::try_from(s.through.max(now)).unwrap_or(i64::MAX);
        s.when
            .upcoming(&self.zone, after, count)
            .into_iter()
            .filter_map(|at| u64::try_from(at).ok())
            .map(|at| self.local(at))
            .collect()
    }
}

/// A schedule's maker as the log and the outbox write it: `person`, or the
/// conversation whose model asked for it. It is never left out, so a
/// writer that forgets it cannot make a model's text the person's.
pub fn maker(author: Option<&crate::store::Id>) -> String {
    author.map_or_else(|| "person".to_string(), ToString::to_string)
}

/// A maker as `maker` writes it.
pub fn maker_from(word: &str) -> Result<Option<crate::store::Id>, String> {
    match word {
        "person" => Ok(None),
        id => crate::store::Id::parse(id)
            .map(Some)
            .ok_or_else(|| format!("{id:?} is not a schedule's maker")),
    }
}

/// What conversation `id` is to a firing, by the window's directory: one
/// it no longer lists, archived, or whose process failed takes none, and
/// its times pass; one running a turn or paused takes it skipped.
pub fn target(directory: &[crate::post::Entry], id: &crate::store::Id) -> Target {
    match directory.iter().find(|e| &e.id == id) {
        None => Target::Away,
        Some(e) if e.archived || e.failed => Target::Away,
        Some(e) if e.state == "running" => Target::Live(Some("a turn was still running".into())),
        Some(e) if e.state == "paused" => Target::Live(Some("the conversation was paused".into())),
        Some(_) => Target::Live(None),
    }
}

/// What a firing's conversation is to the window.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Target {
    /// It takes the firing; why it starts no turn, when it does not.
    Live(Option<String>),
    /// The human archived it, its process failed, or the store no longer
    /// lists it: nothing fires to it.
    Away,
}

/// The composer's schedule commands (DESIGN.md §3).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// `/schedule [catch-up] WHEN TEXT`: a schedule for the open
    /// conversation.
    Add {
        when: String,
        text: String,
        catch_up: bool,
    },
    /// `/schedules`: every schedule, listed.
    List,
    /// `/unschedule ID`: remove one.
    Remove(String),
}

/// The command `text` is, when it is one of the schedule commands, or why
/// it is malformed.
pub fn command(text: &str) -> Option<Result<Command, String>> {
    let text = text.trim();
    let (word, rest) = text
        .split_once(char::is_whitespace)
        .map_or((text, ""), |(word, rest)| (word, rest.trim_start()));
    match word {
        "/schedules" if rest.is_empty() => Some(Ok(Command::List)),
        "/schedules" => Some(Err("/schedules takes nothing".into())),
        "/unschedule" if !rest.is_empty() && !rest.contains(char::is_whitespace) => {
            Some(Ok(Command::Remove(rest.to_string())))
        }
        "/unschedule" => Some(Err("/unschedule takes one schedule's id".into())),
        "/schedule" => Some(add(rest)),
        _ => None,
    }
}

const SCHEDULE_USAGE: &str = "/schedule [catch-up] WHEN TEXT, WHEN five cron fields (minute hour day month weekday) or one local time YYYY-MM-DDTHH:MM";

/// `/schedule`'s arguments.
fn add(rest: &str) -> Result<Command, String> {
    let (catch_up, rest) = match rest.strip_prefix("catch-up") {
        Some(after) if after.starts_with(char::is_whitespace) => (true, after.trim_start()),
        _ => (false, rest),
    };
    let words: Vec<&str> = rest.split_whitespace().collect();
    let fields = match words.first() {
        Some(first) if first.len() == 16 && first.contains('T') => 1,
        _ => 5,
    };
    if words.len() <= fields {
        return Err(format!("usage: {SCHEDULE_USAGE}"));
    }
    let when = words.get(..fields).unwrap_or_default().join(" ");
    When::parse(&when)?;
    // The text as written, past the fields.
    let mut text = rest;
    for _ in 0..fields {
        text = text
            .trim_start()
            .split_once(char::is_whitespace)
            .map_or("", |(_, after)| after);
    }
    Ok(Command::Add {
        when,
        text: text.trim().to_string(),
        catch_up,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    fn utc(text: &str) -> i64 {
        td_civil::civil_to_unix_utc(&td_civil::parse_ymd_hm(text).unwrap()).unwrap()
    }

    fn shown(at: i64) -> String {
        td_civil::format_rfc3339_utc(at)
    }

    #[test]
    fn a_cron_expression_is_five_fields_of_numbers_lists_ranges_and_steps() {
        let cron = Cron::parse("*/15 9-17 * * 1-5").unwrap();
        assert_eq!(cron.minutes, 1 | 1 << 15 | 1 << 30 | 1 << 45);
        assert_eq!(cron.hours, (9..=17).fold(0, |m, h| m | 1 << h));
        assert_eq!(cron.weekdays, 0b0011_1110);
        assert!(!cron.days_restricted && cron.weekdays_restricted);
        // 7 is Sunday as 0 is; `N/S` steps to the field's end.
        assert_eq!(Cron::parse("0 0 * * 7").unwrap().weekdays, 1);
        // A step past the field's end leaves only its start.
        assert_eq!(Cron::parse("*/60 * * * *").unwrap().minutes, 1);
        assert_eq!(Cron::parse("0 */24 * * *").unwrap().hours, 1);
        assert_eq!(Cron::parse("0 0 * * 0,7").unwrap().weekdays, 1);
        assert_eq!(
            Cron::parse("50/5 0 * * *").unwrap().minutes,
            1 << 50 | 1 << 55
        );
        assert_eq!(
            Cron::parse("1,2,10-12/2 0 * * *").unwrap().minutes,
            1 << 1 | 1 << 2 | 1 << 10 | 1 << 12
        );
        for (text, why) in [
            ("* * * *", "five fields"),
            ("* * * * * *", "five fields"),
            ("60 * * * *", "outside 0 to 59"),
            ("* 24 * * *", "outside 0 to 23"),
            ("* * 0 * *", "outside 1 to 31"),
            ("* * * 13 *", "outside 1 to 12"),
            ("* * * * 8", "outside 0 to 7"),
            ("* * * jan *", "not a number"),
            ("@daily", "five fields"),
            ("5-1 * * * *", "backwards"),
            ("*/0 * * * *", "step"),
            ("*/+5 * * * *", "step"),
            ("*/1000 * * * *", "step"),
            ("1,,2 * * * *", "not a number"),
            ("-1 * * * *", "not a number"),
        ] {
            let err = Cron::parse(text).unwrap_err();
            assert!(err.contains(why), "{text}: {err}");
        }
    }

    #[test]
    fn a_when_is_a_cron_expression_or_one_local_time() {
        assert!(matches!(When::parse(" 0 9 * * 1 ").unwrap(), When::Cron(_)));
        assert_eq!(
            When::parse("2026-10-09T08:30").unwrap(),
            When::At(Civil::new(2026, 10, 9, 8, 30, 0))
        );
        for text in [
            "2026-10-09 08:30",
            "2026-10-09T8:30",
            "2026-02-30T08:30",
            "tomorrow",
        ] {
            assert!(When::parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn the_next_firings_follow_the_expression_in_utc() {
        let zone = Zone::utc();
        let weekdays = When::parse("30 9 * * 1-5").unwrap();
        // 2026-10-09 is a Friday.
        let times: Vec<String> = weekdays
            .upcoming(&zone, utc("2026-10-09 10:00"), 3)
            .into_iter()
            .map(shown)
            .collect();
        assert_eq!(
            times,
            [
                "2026-10-12T09:30:00Z",
                "2026-10-13T09:30:00Z",
                "2026-10-14T09:30:00Z"
            ]
        );
        // Exactly at a firing, the next is the one after.
        assert_eq!(
            weekdays
                .next_after(&zone, utc("2026-10-12 09:30"))
                .map(shown),
            Some("2026-10-13T09:30:00Z".into())
        );
        // Both day fields restricted: either suffices, as in cron.
        let either = When::parse("0 0 13 * 5").unwrap();
        let times: Vec<String> = either
            .upcoming(&zone, utc("2026-11-01 00:00"), 3)
            .into_iter()
            .map(shown)
            .collect();
        assert_eq!(
            times,
            [
                "2026-11-06T00:00:00Z",
                "2026-11-13T00:00:00Z",
                "2026-11-20T00:00:00Z"
            ]
        );
        // A leap day is found across a century year that is not a leap
        // year, and one that must also be a Sunday decades on; a date
        // that never comes is never found, nor anything past 9999.
        let later = |text: &str, after: &str| {
            When::parse(text)
                .unwrap()
                .next_after(&zone, utc(after))
                .map(shown)
        };
        assert_eq!(
            later("0 0 29 2 */7", "2032-03-01 00:00"),
            Some("2060-02-29T00:00:00Z".into())
        );
        assert_eq!(
            later("0 0 */30 2 1", "2027-02-02 00:00"),
            Some("2038-02-01T00:00:00Z".into())
        );
        assert_eq!(
            When::parse("* * * * *")
                .unwrap()
                .next_after(&zone, i64::MAX),
            None
        );
        assert_eq!(
            When::parse("0 0 29 2 *")
                .unwrap()
                .next_after(&zone, utc("2097-03-01 00:00"))
                .map(shown),
            Some("2104-02-29T00:00:00Z".into())
        );
        assert_eq!(
            When::parse("0 0 31 2 *").unwrap().next_after(&zone, 0),
            None
        );
        // Once: before it, and never after.
        let once = When::parse("2026-10-09T08:30").unwrap();
        assert_eq!(
            once.upcoming(&zone, utc("2026-10-01 00:00"), 3)
                .into_iter()
                .map(shown)
                .collect::<Vec<_>>(),
            ["2026-10-09T08:30:00Z"]
        );
        assert_eq!(once.next_after(&zone, utc("2026-10-09 08:30")), None);
    }

    /// US Eastern for 2026 as a TZif v2 file (RFC 9636): clocks go forward
    /// an hour at 2026-03-08 02:00 local and back at 2026-11-01 02:00, the
    /// footer's rule after.
    fn new_york() -> Zone {
        fn header(out: &mut Vec<u8>, times: usize, kinds: usize, names: usize) {
            out.extend_from_slice(b"TZif2");
            out.extend_from_slice(&[0; 15]);
            for count in [0, 0, 0, times, kinds, names] {
                out.extend_from_slice(&(count as u32).to_be_bytes());
            }
        }
        let times = [(utc("2026-03-08 07:00"), 1u8), (utc("2026-11-01 06:00"), 0)];
        let kinds = [(-18_000i32, false, "EST"), (-14_400, true, "EDT")];
        let mut out = Vec::new();
        header(&mut out, 0, 1, 1);
        out.extend_from_slice(&[0; 7]);
        let names: Vec<u8> = kinds
            .iter()
            .flat_map(|(_, _, name)| name.bytes().chain([0]))
            .collect();
        header(&mut out, times.len(), kinds.len(), names.len());
        for (time, _) in times {
            out.extend_from_slice(&time.to_be_bytes());
        }
        for (_, index) in times {
            out.push(index);
        }
        let mut index = 0;
        for (offset, daylight, name) in kinds {
            out.extend_from_slice(&offset.to_be_bytes());
            out.push(u8::from(daylight));
            out.push(index as u8);
            index += name.len() + 1;
        }
        out.extend_from_slice(&names);
        out.extend_from_slice(b"\nEST5EDT,M3.2.0,M11.1.0\n");
        Zone::from_tzif(&out).unwrap()
    }

    #[test]
    fn a_local_time_in_a_gap_is_skipped_and_one_that_repeats_fires_once() {
        let zone = new_york();
        // 02:30 does not exist on 2026-03-08: that day is skipped.
        let times: Vec<String> = When::parse("30 2 * * *")
            .unwrap()
            .upcoming(&zone, utc("2026-03-07 12:00"), 2)
            .into_iter()
            .map(shown)
            .collect();
        assert_eq!(times, ["2026-03-09T06:30:00Z", "2026-03-10T06:30:00Z"]);
        // 01:30 happens twice on 2026-11-01: it fires at the first.
        let times: Vec<String> = When::parse("30 1 * * *")
            .unwrap()
            .upcoming(&zone, utc("2026-10-31 12:00"), 2)
            .into_iter()
            .map(shown)
            .collect();
        assert_eq!(times, ["2026-11-01T05:30:00Z", "2026-11-02T06:30:00Z"]);
        // A frequent schedule is quiet through the repeated hour: after
        // 01:50 EDT it fires next at 02:00 EST, an hour and ten minutes on.
        assert_eq!(
            When::parse("*/15 * * * *")
                .unwrap()
                .next_after(&zone, utc("2026-11-01 05:50"))
                .map(shown),
            Some("2026-11-01T07:00:00Z".into())
        );
        // The walk starts from the local date: 22:30 EDT on 31 May is
        // 02:30 UTC on 1 June, and 23:00 that evening is still to come.
        assert_eq!(
            When::parse("0 23 * * *")
                .unwrap()
                .next_after(&zone, utc("2026-06-01 02:30"))
                .map(shown),
            Some("2026-06-01T03:00:00Z".into())
        );
        // A single time in the gap never fires.
        assert_eq!(
            When::parse("2026-03-08T02:30")
                .unwrap()
                .next_after(&zone, 0),
            None
        );
    }

    fn conversation(n: u8) -> crate::store::Id {
        crate::store::Id::parse(&format!("{n:032x}")).unwrap()
    }

    #[test]
    fn the_composer_commands_are_parsed_or_said_malformed() {
        assert_eq!(
            command("/schedule 30 9 * * 1-5 check the  nightly build"),
            Some(Ok(Command::Add {
                when: "30 9 * * 1-5".into(),
                text: "check the  nightly build".into(),
                catch_up: false
            }))
        );
        assert_eq!(
            command(" /schedule catch-up 2026-10-09T08:30 ship it\nand say so "),
            Some(Ok(Command::Add {
                when: "2026-10-09T08:30".into(),
                text: "ship it\nand say so".into(),
                catch_up: true
            }))
        );
        assert_eq!(command("/schedules"), Some(Ok(Command::List)));
        assert!(matches!(command("/schedules all"), Some(Err(_))));
        assert_eq!(
            command("/unschedule 0a1b"),
            Some(Ok(Command::Remove("0a1b".into())))
        );
        for text in [
            "/schedule",
            "/schedule 30 9 * * 1-5",
            "/schedule 2026-10-09T08:30",
            "/schedule 61 9 * * * late",
            "/schedule tomorrow at nine",
            "/unschedule",
            "/unschedule a b",
        ] {
            assert!(matches!(command(text), Some(Err(_))), "{text}");
        }
        for text in ["/schedulex 1 2 3 4 5 x", "schedule 1 2 3 4 5 x", "hello"] {
            assert_eq!(command(text), None, "{text}");
        }
    }

    #[test]
    fn schedules_are_kept_whole_and_removed_by_their_id() {
        let scratch = crate::store::tests::Scratch::new("schedules");
        let state = scratch.state();
        let now = utc("2026-10-08 12:00") as u64;
        let (mut schedules, problem) = Schedules::load(&state, Zone::utc());
        assert_eq!(problem, None);
        let made = schedules
            .add(
                "0 9 * * *",
                conversation(1),
                "check \"it\"".into(),
                true,
                None,
                now,
            )
            .unwrap();
        assert!(id_ok(&made.id));
        assert_eq!(made.through, now);
        assert_eq!(
            schedules.upcoming(&made, now, 2),
            ["2026-10-09 09:00", "2026-10-10 09:00"]
        );
        let other = schedules
            .add(
                "2026-10-09T08:30",
                conversation(2),
                "once".into(),
                false,
                Some(conversation(1)),
                now,
            )
            .unwrap();
        assert_eq!(schedules.soonest(), Some(utc("2026-10-09 08:30") as u64));
        let (again, problem) = Schedules::load(&state, Zone::utc());
        assert_eq!(problem, None);
        assert_eq!(again.list(), schedules.list());
        // Refused: never again, no text, a bad expression.
        for (when, text) in [
            ("2026-10-01T08:30", "x"),
            ("0 9 * * *", " "),
            ("0 9 * *", "x"),
        ] {
            assert!(schedules
                .add(when, conversation(1), text.into(), false, None, now)
                .is_err());
        }
        assert!(schedules.remove("").is_err());
        assert!(schedules.remove("zz").is_err());
        let removed = schedules.remove(&other.id[..4]).unwrap();
        assert_eq!(removed, other);
        let (again, _) = Schedules::load(&state, Zone::utc());
        assert_eq!(again.list(), [made]);
    }

    #[test]
    fn a_file_that_cannot_be_read_is_set_aside() {
        let scratch = crate::store::tests::Scratch::new("schedules-aside");
        let state = scratch.state();
        std::fs::write(state.root().join(SCHEDULES), "[{\"id\":1}]").unwrap();
        let (schedules, problem) = Schedules::load(&state, Zone::utc());
        assert!(schedules.list().is_empty());
        let problem = problem.unwrap();
        assert!(
            problem.contains("could not be read") && problem.contains("moved"),
            "{problem}"
        );
        assert!(!state.root().join(SCHEDULES).exists());
    }

    #[test]
    fn a_firing_is_journaled_once_and_a_missed_one_dropped_unless_it_catches_up() {
        let scratch = crate::store::tests::Scratch::new("schedules-due");
        let state = scratch.state();
        let made = utc("2026-10-08 00:00") as u64;
        let (mut schedules, _) = Schedules::load(&state, Zone::utc());
        let hourly = schedules
            .add(
                "0 * * * *",
                conversation(1),
                "hourly".into(),
                false,
                None,
                made,
            )
            .unwrap();
        let caught = schedules
            .add(
                "30 * * * *",
                conversation(2),
                "caught".into(),
                true,
                None,
                made,
            )
            .unwrap();
        let once = schedules
            .add(
                "2026-10-08T01:10",
                conversation(1),
                "once".into(),
                false,
                None,
                made,
            )
            .unwrap();
        let live = |_: &crate::store::Id| Target::Live(None);
        // Started at 05:45, hours after: the missed hourly is dropped, the
        // one that catches up fires once, and the one-off missed goes.
        let start = utc("2026-10-08 05:45") as u64;
        let due = schedules.due(start, &live).unwrap();
        let fired: Vec<&str> = due.iter().map(|d| d.text.as_str()).collect();
        assert_eq!(fired, ["caught"]);
        assert_eq!(due[0].to, conversation(2));
        let ids: Vec<&str> = schedules.list().iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, [hourly.id.as_str(), caught.id.as_str()]);
        assert!(schedules.list().iter().all(|s| s.through == start));
        assert!(!schedules.list().iter().any(|s| s.id == once.id));
        assert_eq!(schedules.soonest(), Some(utc("2026-10-08 06:00") as u64));
        // Nothing more until then; then the hourly fires, once, however
        // late.
        assert!(schedules.due(start + 60, &live).unwrap().is_empty());
        let late = utc("2026-10-08 08:20") as u64;
        let fired: Vec<String> = schedules
            .due(late, &live)
            .unwrap()
            .into_iter()
            .map(|d| d.text)
            .collect();
        assert_eq!(fired, ["hourly", "caught"]);
        // Journaled: a restart, or the clock set back, fires none again.
        let (mut again, _) = Schedules::load(&state, Zone::utc());
        assert!(again.due(late, &live).unwrap().is_empty());
        assert!(again.due(late - 3600, &live).unwrap().is_empty());
        // A conversation away fires nothing, its times passing; one busy
        // takes the firing to log it skipped.
        let away = |id: &crate::store::Id| match *id == conversation(1) {
            true => Target::Away,
            false => Target::Live(Some("a turn was still running".into())),
        };
        let later = utc("2026-10-08 09:40") as u64;
        let due = again.due(later, &away).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].text, "caught");
        assert_eq!(due[0].skipped.as_deref(), Some("a turn was still running"));
        assert!(again.list().iter().all(|s| s.through == later));
    }

    /// A startup whose journal could not be written is still a startup
    /// when tried again: what was missed is dropped, not fired.
    #[test]
    fn a_startup_that_could_not_journal_still_drops_what_was_missed() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = crate::store::tests::Scratch::new("schedules-retry");
        let state = scratch.state();
        let made = utc("2026-10-08 00:00") as u64;
        let (mut schedules, _) = Schedules::load(&state, Zone::utc());
        schedules
            .add(
                "0 * * * *",
                conversation(1),
                "hourly".into(),
                false,
                None,
                made,
            )
            .unwrap();
        let (mut schedules, _) = Schedules::load(&state, Zone::utc());
        let live = |_: &crate::store::Id| Target::Live(None);
        let root = state.root().to_path_buf();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o500)).unwrap();
        let start = utc("2026-10-08 05:45") as u64;
        let failed = schedules.due(start, &live);
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(failed.is_err());
        assert!(schedules.due(start + 60, &live).unwrap().is_empty());
        assert_eq!(schedules.list()[0].through, start + 60);
    }

    #[test]
    fn the_window_says_what_a_conversation_is_to_a_firing() {
        let entry = |n: u8, state: &str, failed: bool, archived: bool| crate::post::Entry {
            id: conversation(n),
            state: state.into(),
            failed,
            archived,
        };
        let directory = [
            entry(1, "idle", false, false),
            entry(2, "running", false, false),
            entry(3, "paused", false, false),
            entry(4, "idle", false, true),
            entry(5, "failed", true, false),
        ];
        let of = |n: u8| target(&directory, &conversation(n));
        assert_eq!(of(1), Target::Live(None));
        assert!(matches!(of(2), Target::Live(Some(why)) if why.contains("running")));
        assert!(matches!(of(3), Target::Live(Some(why)) if why.contains("paused")));
        for n in [4, 5, 6] {
            assert_eq!(of(n), Target::Away, "{n}");
        }
        assert_eq!(maker_from(&maker(None)), Ok(None));
        let one = conversation(1);
        assert_eq!(maker_from(&maker(Some(&one))), Ok(Some(one)));
        assert!(maker_from("").is_err() && maker_from("model").is_err());
    }
}
