//! The flight specialist: the agent that searches, books and plans trips,
//! offered to the main agent as `ask_flights`.
//!
//! Two halves. `FLIGHT_PREAMBLE` is the working half of what used to be
//! the main prompt's flight section: how to search. `guidance` is the
//! presentation half: how to write the answer, returned with the findings
//! so that only a flight turn ever pays for it.

use crate::agent::{fare_market, rules_for_available_tools, AgentDeps};
use crate::specialist::{Finding, Specialist, SPECIALIST_BUDGET};
use crate::tools::trips::{
    AddTripOptionTool, AddTripSegmentTool, ChooseTripOptionTool, DeleteTripTool,
    DropTripSegmentTool, FinaliseTripTool, ShowTripTool, UpdateTripSegmentTool,
};
use rig::client::CompletionClient;

/// Model calls one flight question may take. A flexible return with a
/// booking link is search, links, answer: three. Twelve leaves room for a
/// multi-city plan built segment by segment.
pub const FLIGHT_TURNS: usize = 12;

/// The tools whose rules the flight prompt may describe conditionally.
pub const FLIGHT_TOOLS: &[&str] = &["flight_booking_links", "create_booking_link"];

/// The trip-planning tools: a finding from any of them brings the trip
/// presentation rules.
const TRIP_TOOLS: &[&str] = &[
    "add_trip_segment",
    "add_trip_option",
    "choose_trip_option",
    "show_trip",
    "update_trip_segment",
    "drop_trip_segment",
    "delete_trip",
    "finalise_trip",
];

pub const FLIGHT_PREAMBLE: &str = "\
You are Scout's flight desk. Another agent hands you a brief - a route, \
dates, passengers, sometimes an offer id - and you search, book or plan \
with the tools you have, then report back. You never speak to the \
traveller and you never buy anything.

Rules:
- Use search_flights for every fare question - never guess a price and \
never answer from memory. It asks the airlines directly, so its prices, \
times and flight numbers are current. Airports go in as IATA codes - work \
them out yourself rather than asking: Amsterdam is AMS, Lisbon LIS, London \
LHR (or LON for all its airports) - and dates as YYYY-MM-DD.
- If the brief lacks something a search needs - a destination, a date, the \
number of passengers - do not guess it and do not search: say in your \
report exactly what is missing, so it can be asked for.
- When the brief says the dates are flexible, or asks whether another day \
is cheaper, pass flex_days (max 3) instead of searching each date \
yourself: it prices the whole window in one call. Do NOT pass flex_days \
otherwise - each day in the window is a separate paid search. Nobody can \
price a whole month; a week either side is the most that can be checked.
- When flight_booking_links is available, and the brief names an ignav \
offer_id to book, call it with that offer_id. It re-checks the fare and \
returns the airline's page and any resellers.
- When create_booking_link is available, and the brief asks to book a \
Duffel row, call it.
- If the brief asks to book and you have no booking tool, do not search \
again: say in your report that Scout cannot book, and that the traveller \
buys from the airline with the numbers already found.
- When the brief is about more than one flight - a multi-city route, or a \
trip being assembled over several messages - build it with the trip tools \
rather than holding it in your head: add_trip_segment for each leg the \
moment you have a route and a date, add_trip_option for each flight \
found. A segment is one direction on one date, so a return is two \
segments. When a date or a leg changes, call update_trip_segment on that \
one segment. NEVER delete the trip and build it again, and never drop and \
re-add a segment to change it: both throw away every option parked on \
every other segment, and dropping renumbers everything after it. If the \
traveller is undecided between flights, park each with add_trip_option \
and decided=false; several options may sit on one segment. Finalising is \
the only thing that produces current prices and it costs a search per \
segment, so call finalise_trip when the brief says the trip is settled, \
not to check on it.
- Every trip tool hands back the whole trip. If a call failed, the trip \
did NOT change. When you make several edits, trust each call's own \
'changed' line over the snapshot beside it - a snapshot is from the \
moment that call ran - and believe the last snapshot, or call show_trip \
once at the end.
- Finish with one or two plain sentences: what you searched or changed, \
and any caveat - nothing flew that day, the window that was covered, a \
fare that moved. Give no prices, times or links in those sentences: the \
tool results travel back with your report and are read from there.";

/// The booking tools this install actually has, for the prompt filter.
fn available_tools(d: &AgentDeps) -> Vec<&'static str> {
    FLIGHT_TOOLS
        .iter()
        .copied()
        .filter(|tool| match *tool {
            "flight_booking_links" => d.ignav.is_some(),
            "create_booking_link" => d.duffel.is_some() && d.links_enabled && d.return_url.is_some(),
            _ => false,
        })
        .collect()
}

/// The nested agent: the flight prompt and every flight-shaped tool, wired
/// as the main agent wired them before the split, with one difference: the
/// provider gate (`duffel || ignav`) now lives on `ask_flights` in the
/// parent, so search and finalise are unconditional here. Built per
/// request, like the main agent, because the tools capture the account.
pub fn build_flight_agent(
    d: &AgentDeps,
    run: &scout_api::RunContext,
    facts: &[(String, String)],
    budget: std::sync::Arc<crate::tools::budget::FlightBudget>,
) -> rig::agent::Agent<rig::providers::openai::completion::CompletionModel> {
    let (account_id, conversation_id) = (run.account_id, run.conversation_id);
    // Priced in the traveller's own currency, or Duffel's euros and
    // Ignav's dollars never get compared. Shared by search and finalise:
    // finalising re-prices through IgnavClient::search, which reads the
    // market; booking_links ignores it, since the id carries its own.
    let ignav = d.ignav.clone().map(|c| match fare_market(facts) {
        Some(market) => c.with_market(&market),
        None => c,
    });
    let mut builder = d
        .llm
        .agent(&d.flight_model)
        .preamble(&rules_for_available_tools(FLIGHT_PREAMBLE, &available_tools(d)))
        .tool(crate::tools::duffel::FlightSearchTool {
            duffel: d.duffel.clone(),
            store: d.store.clone(),
            account_id,
            budget: budget.clone(),
            shown: d.shown.clone(),
            conversation_id,
            ignav: ignav.clone(),
        })
        .tool(FinaliseTripTool {
            store: d.store.clone(),
            account_id,
            duffel: d.duffel.clone(),
            ignav,
            budget,
        })
        // Trip planning needs no provider, but a trip is a flight plan, so
        // it lives with the flights. That is a deliberate change: before
        // the split the trip tools were registered even with no provider;
        // now an install with neither provider has no trip planning, by
        // design (spec decision: everything flight-shaped moves).
        .tool(AddTripSegmentTool { store: d.store.clone(), account_id })
        .tool(AddTripOptionTool {
            store: d.store.clone(),
            account_id,
            shown: d.shown.clone(),
            conversation_id,
        })
        .tool(ChooseTripOptionTool { store: d.store.clone(), account_id })
        .tool(ShowTripTool { store: d.store.clone(), account_id })
        .tool(UpdateTripSegmentTool { store: d.store.clone(), account_id })
        .tool(DropTripSegmentTool { store: d.store.clone(), account_id })
        .tool(DeleteTripTool { store: d.store.clone(), account_id });
    // Where an Ignav row can actually be bought.
    if let Some(ignav) = &d.ignav {
        builder = builder.tool(crate::tools::ignav::BookingLinksTool {
            client: ignav.clone(),
            shown: d.shown.clone(),
            conversation_id,
        });
    }
    // Duffel's hosted checkout: needs Duffel, Links enabled, and somewhere
    // to send people back to.
    if let (Some(duffel), Some(return_url)) =
        (&d.duffel, d.return_url.as_ref().filter(|_| d.links_enabled))
    {
        builder = builder.tool(crate::tools::duffel::BookingLinkTool {
            client: duffel.clone(),
            account_id,
            return_url: return_url.clone(),
        });
    }
    builder.default_max_turns(FLIGHT_TURNS).build()
}

/// The flight agent as the one tool the main agent sees.
pub fn ask_flights(
    d: &AgentDeps,
    run: &scout_api::RunContext,
    facts: &[(String, String)],
    budget: std::sync::Arc<crate::tools::budget::FlightBudget>,
    events: scout_api::EventSink,
    pulse: std::sync::Arc<crate::run::Pulse>,
) -> Specialist<rig::providers::openai::completion::CompletionModel> {
    let markup = crate::agent::markup_rate(d);
    Specialist {
        name: "ask_flights",
        description: "Scout's flight desk. Send it every question about flights, fares, \
                      airport codes, booking a flight, or a trip being planned. It sees nothing \
                      of the conversation, so the brief must be self-contained: route, \
                      dates, passengers, cabin, whether the dates are flexible, and any \
                      offer id the user is pointing at. It returns findings - the real \
                      search and booking results - plus guidance on presenting them."
            .to_string(),
        agent: build_flight_agent(d, run, facts, budget),
        events,
        pulse,
        guidance: Box::new(move |findings: &[Finding]| guidance(findings, markup)),
        budget: SPECIALIST_BUDGET,
    }
}

/// The presentation rules that apply to what the specialist did, one
/// block per section, each included only when its trigger is present.
///
/// Returned with the findings rather than kept in the main prompt, so a
/// shopping turn never pays for a word of it. Written for the agent that
/// talks to the traveller.
pub fn guidance(findings: &[crate::specialist::Finding], markup_rate: f64) -> Vec<String> {
    // A failed finding is an error string, not a result; nothing below
    // applies to it.
    let ok = || findings.iter().filter(|f| !f.failed);
    let ran = |tool: &str| ok().any(|f| f.tool == tool);
    let searched = ran("search_flights");
    let windowed = ok().any(|f| {
        f.tool == "search_flights"
            && f.output.get("by_date").and_then(|v| v.as_array()).is_some_and(|a| !a.is_empty())
    });
    let planned = ok().any(|f| TRIP_TOOLS.contains(&f.tool.as_str()));
    let mut out = Vec::new();
    // Whether any section with prices in it was pushed: the fee line
    // applies to prices, so it keys on these and not on the failed block.
    let mut priced = false;
    if searched {
        out.push(SEARCH_GUIDANCE.to_string());
        priced = true;
    }
    if windowed {
        out.push(WINDOW_GUIDANCE.to_string());
        priced = true;
    }
    if ran("flight_booking_links") {
        out.push(IGNAV_LINKS_GUIDANCE.to_string());
        priced = true;
    }
    if ran("create_booking_link") {
        out.push(DUFFEL_LINK_GUIDANCE.to_string());
        priced = true;
    }
    if planned {
        out.push(TRIP_GUIDANCE.to_string());
        priced = true;
    }
    // A failed finding travels back too, and without this the parent has
    // an error string and no word on what `failed` means.
    if findings.iter().any(|f| f.failed) {
        out.push(FAILED_GUIDANCE.to_string());
    }
    if priced && markup_rate > 0.0 {
        out.push(format!(
            "Every flight price in these findings ALREADY includes a {} booking fee, which is \
             what the checkout charges. Quote the numbers unchanged and say once, in plain \
             words, that prices include that booking fee. Never add it on yourself.",
            crate::agent::percentage(markup_rate)
        ));
    }
    out
}

const SEARCH_GUIDANCE: &str = "\
Presenting search_flights findings: take every number verbatim - cheapest \
and fastest are ranked in Rust and are not yours to recompute - and quote \
the route field so the reply cannot drift onto a route nobody searched. \
Present the picks under their own headings - Cheapest, Fastest, Best balance \
- one to two options each, in that order, and offer every one you are \
given: they are already chosen from hundreds and no option appears under \
two headings, so anything you drop is a choice removed for no reason. When \
a group is empty, leave the heading out, and when one option is both \
cheapest and quickest say so. Best balance is the option closest to being \
both the cheapest and the quickest, worked out in Rust; do not recompute \
it or argue with it. Every option carries price_status: 'bookable' \
is a live offer, quote it as a price; 'approximate' and 'unconfirmed' are \
fares seen elsewhere that still have to be checked on the seller's page - \
quote those as 'from EUR 180', say where they came from, and NEVER present \
one as a plain price or as 'the cheapest' without saying it is not bookable. \
When self_transfer is true the trip is two separate tickets: the traveller \
re-checks their bags and carries the risk if the first flight is late, so \
say that every time it appears. departing_at_local and arriving_at_local are \
each in the local time of their own airport, so NEVER subtract them to work \
out how long a flight takes: LHR 10:03 to JFK 13:01 is a 7h58m flight, not \
a 2h58m one. Give journey length from the duration field. For any flight \
with a change of plane, name the connection airport and the wait from the \
connections list - 'changes at PVG, 3h 20m'; never work a layover out from \
flight numbers or your own knowledge. When changes_airport is true, say so \
loudly: the traveller lands at one airport and departs from another. A \
connection whose layover is null means the offer did not state the times - \
say that rather than guessing, and do not offer to go and look it up \
elsewhere. Every leg carries an itinerary line already drawn for you - \
'AMS 20:15 15.09 ✈ PVG 3h 20m ✈ HKG 20:35 16.09'. Put it on its own line \
under the option it belongs to, copied EXACTLY as given: do not retype, \
reorder, translate or rebuild it. A return \
trip has one such line per leg, outbound first. Say the price, the airline \
and what matters about the option, then the line. The line replaces listing \
the times in prose. Prices are the whole trip for all passengers. Say what \
the notes say when they matter: a cheapest option that takes hours longer, \
bags that are not included, offers left out for being in another currency. \
found: 0 means nothing flies that route that day - say so plainly rather \
than guessing at alternatives. Flights have no product page to link; their \
count is set by the rows the search returned. Each option is its own block, \
separated by a blank line. These prices expire within minutes: in a later \
message, ask ask_flights again rather than repeating a fare from earlier in \
the conversation.";

const FAILED_GUIDANCE: &str = "\
A finding with failed: true is the error the tool returned, not a result: \
say plainly that that lookup failed and what the error says in a few words, \
and do not present anything from it as a price, a time or a link.";

const WINDOW_GUIDANCE: &str = "\
Presenting by_date: the search covered a window and by_date is the cheapest \
fare per day. Present it as a short list, cheapest day marked, and say which \
days were covered - the route field ends in ±N. A note may say some days \
could not be afforded; then say which window the answer actually covers.";

const IGNAV_LINKS_GUIDANCE: &str = "\
Presenting flight_booking_links: give the airline's link first and name any \
reseller that is cheaper, with both prices, rather than choosing for them. \
The links open with the flight already selected, so there is nothing to \
re-enter. The fare was re-checked at that moment: if a note says it has \
risen or fallen, tell them the new price before they open anything and \
never repeat the old one.";

const DUFFEL_LINK_GUIDANCE: &str = "\
Presenting create_booking_link: give the link on its own line and say \
plainly what it is - Duffel's own checkout, where they pick the flight and \
pay; Scout never sees passenger or card details. It CANNOT be pre-filled - \
it opens on its own search box - so repeat the route, date and price for \
them to enter, and say the link is single-use and short-lived. Never \
re-send an old one.";

const TRIP_GUIDANCE: &str = "\
Presenting a trip: the findings carry the whole trip as the tools returned \
it. When not_ready is present the trip cannot be priced and its reason \
says which segment is missing what; when it is absent the trip is \
complete. Say what it says, and never describe a trip from memory. Quote a \
trip's prices as of when each option was parked, never as a current total: \
only finalise_trip re-prices them. When finalise_trip ran, present both \
totals it returns and never drop the note about separate tickets: a link \
per segment is a ticket per segment, and the traveller carries the risk at \
every join. If the single-ticket total is missing, say that it is missing - \
it is not evidence that separate booking is better.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::specialist::Finding;
    use serde_json::json;

    fn ran(tool: &str, output: serde_json::Value) -> Finding {
        Finding { tool: tool.to_string(), args: json!({}), output, failed: false }
    }

    #[test]
    fn a_failed_search_brings_no_search_rules() {
        // Only the block that says what failed: true means, and no fee
        // line, since there is no price for a fee to apply to.
        let failed = Finding { failed: true, ..ran("search_flights", json!("duffel api error (status 429)")) };
        let g = guidance(&[failed], 0.03);
        assert_eq!(g.len(), 1, "got: {g:?}");
        assert!(g[0].contains("failed: true"), "got: {}", g[0]);
        assert!(!g[0].contains("Cheapest, Fastest"), "got: {}", g[0]);
        assert!(!g[0].contains("booking fee"), "got: {}", g[0]);
    }

    #[test]
    fn a_failed_search_beside_a_good_one_brings_both_rules_and_the_fee() {
        let failed = Finding { failed: true, ..ran("search_flights", json!("duffel api error (status 429)")) };
        let g = guidance(&[ran("search_flights", json!({"route": "AMS-LIS"})), failed], 0.03);
        let text = g.join("\n");
        assert!(text.contains("Cheapest, Fastest"), "got: {text}");
        assert_eq!(g.iter().filter(|s| s.contains("failed: true")).count(), 1, "got: {g:?}");
        assert!(text.contains("3% booking fee"), "got: {text}");
        // The fee line closes the guidance; the failed block comes before it.
        assert!(g.last().unwrap().contains("booking fee"), "got: {g:?}");
    }

    #[test]
    fn nothing_ran_nothing_to_say() {
        assert!(guidance(&[], 0.0).is_empty());
    }

    #[test]
    fn a_search_brings_the_option_rules_and_only_a_window_brings_the_by_date_rules() {
        let plain = guidance(&[ran("search_flights", json!({"route": "AMS-LIS", "by_date": []}))], 0.0);
        let text = plain.join("\n");
        assert!(text.contains("Cheapest, Fastest, Best balance"), "got: {text}");
        assert!(text.contains("itinerary"), "got: {text}");
        assert!(text.contains("price_status"), "got: {text}");
        assert!(text.contains("self_transfer"), "got: {text}");
        // Expiry is the parent's problem: it is the one with a conversation
        // to repeat a fare from. And the details the move dropped.
        assert!(text.contains("expire within minutes"), "got: {text}");
        assert!(text.contains("7h58m"), "got: {text}");
        assert!(text.contains("do not recompute"), "got: {text}");
        assert!(!text.contains("by_date"), "no window, no window rules: {text}");

        let window = guidance(
            &[ran("search_flights", json!({"route": "AMS-LIS ±2", "by_date": [{"date": "2026-10-11"}]}))],
            0.0,
        );
        assert!(window.join("\n").contains("by_date"), "got: {window:?}");
    }

    #[test]
    fn booking_and_trip_rules_come_with_their_tools() {
        let text = guidance(&[ran("flight_booking_links", json!({}))], 0.0).join("\n");
        assert!(text.contains("airline's link first"), "got: {text}");
        assert!(!text.contains("Cheapest, Fastest"), "no search, no option rules: {text}");

        let text = guidance(&[ran("create_booking_link", json!({}))], 0.0).join("\n");
        assert!(text.contains("single-use"), "got: {text}");

        for tool in ["add_trip_segment", "show_trip", "finalise_trip", "delete_trip"] {
            let text = guidance(&[ran(tool, json!({}))], 0.0).join("\n");
            assert!(text.contains("not_ready"), "{tool}: {text}");
            assert!(text.contains("separate tickets"), "{tool}: {text}");
        }
    }

    #[test]
    fn every_trip_tool_brings_the_trip_rules_and_a_lookalike_does_not() {
        for tool in TRIP_TOOLS {
            let text = guidance(&[ran(tool, json!({}))], 0.0).join("\n");
            assert!(text.contains("not_ready"), "{tool}: {text}");
        }
        // A name with 'trip' in it is not a trip tool.
        let text = guidance(&[ran("round_trip_helper", json!({}))], 0.0).join("\n");
        assert!(!text.contains("not_ready"), "got: {text}");
    }

    #[test]
    fn the_fee_is_stated_only_when_charged() {
        let free = guidance(&[ran("search_flights", json!({}))], 0.0).join("\n");
        assert!(!free.contains("booking fee"), "got: {free}");
        let charged = guidance(&[ran("search_flights", json!({}))], 0.03).join("\n");
        assert!(charged.contains("3% booking fee"), "got: {charged}");
    }

    #[test]
    fn each_section_appears_once_however_many_times_its_tool_ran() {
        let g = guidance(
            &[ran("search_flights", json!({})), ran("search_flights", json!({}))],
            0.0,
        );
        assert_eq!(g.iter().filter(|s| s.contains("Cheapest, Fastest")).count(), 1);
    }

    #[test]
    fn the_flight_prompt_searches_and_the_guidance_presents() {
        // The split is the point: a rule in the wrong half is either paid
        // for by every shopping turn or missing from every flight turn.
        for word in ["itinerary", "Cheapest, Fastest", "price_status", "self_transfer"] {
            assert!(!FLIGHT_PREAMBLE.contains(word), "{word:?} is presentation, it belongs in guidance");
        }
        for word in ["flex_days", "add_trip_segment", "IATA", "no prices", "what is missing", "cannot book"] {
            assert!(FLIGHT_PREAMBLE.contains(word), "{word:?} is missing from the flight prompt");
        }
        // The desk has no earlier brief to reuse a fare from; expiry is a
        // rule for the agent with a conversation.
        assert!(!FLIGHT_PREAMBLE.contains("expires"), "expiry belongs in the guidance, not the flight prompt");
    }

    #[test]
    fn the_booking_rules_drop_with_their_tools() {
        let none = crate::agent::rules_for_available_tools(FLIGHT_PREAMBLE, &[]);
        assert!(!none.contains("flight_booking_links"), "got: {none}");
        assert!(!none.contains("create_booking_link"), "got: {none}");
        assert!(none.contains("search_flights"), "the search rule is unconditional");

        let all = crate::agent::rules_for_available_tools(FLIGHT_PREAMBLE, FLIGHT_TOOLS);
        assert!(all.contains("flight_booking_links") && all.contains("create_booking_link"));
    }

    #[test]
    fn every_conditional_rule_in_the_flight_prompt_is_a_flight_tool() {
        for rule in FLIGHT_PREAMBLE.split("\n- ").skip(1) {
            if let Some((tool, _)) = rule.strip_prefix("When ").and_then(|r| r.split_once(" is available,")) {
                assert!(FLIGHT_TOOLS.contains(&tool), "{tool:?} is offered conditionally but nothing gates it");
            }
        }
    }

    #[test]
    fn the_flight_desk_is_named_ask_flights_and_asks_for_a_brief() {
        // No provider is needed to build it: the tool is built from the
        // deps, and the model is only reached when it is called.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.duckdb");
        let core = crate::core::Core::start(crate::config::Config::for_test(path.to_str().unwrap()), None).unwrap();
        let run = scout_api::RunContext {
            account_id: 1,
            conversation_id: 1,
            reply_to: Some(scout_api::ReplyTo::telegram(1)),
            title_source: None,
        };
        let (events, _seen) = tokio::sync::mpsc::unbounded_channel();
        let budget = std::sync::Arc::new(crate::tools::budget::FlightBudget::default());
        let pulse = std::sync::Arc::new(crate::run::Pulse::default());

        let tool = ask_flights(&core.deps, &run, &[], budget, events, pulse);

        assert_eq!(rig::tool::Tool::name(&tool), "ask_flights");
        let d = rig::tool::Tool::description(&tool);
        for word in ["route", "dates", "passengers", "offer id"] {
            assert!(d.contains(word), "the parent must be told to put {word:?} in the brief: {d}");
        }
        let p = rig::tool::Tool::parameters(&tool);
        assert_eq!(p["required"], serde_json::json!(["brief"]));
    }
}
