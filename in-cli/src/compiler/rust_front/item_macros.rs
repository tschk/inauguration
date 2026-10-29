//! Source-owned expansion of lexical item macros before type catalog construction.
//!
//! Unsupported fragments and ambiguous matches fail closed; expansion never calls rustc.
#[path = "item_macros/hygiene.rs"]
mod hygiene;

use super::type_aliases::{Presence, attrs_presence, item_presence};
use hygiene::Hygiene;
use proc_macro2::{Delimiter, Group, TokenStream, TokenTree};
use quote::ToTokens;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::rc::Rc;
use syn::parse::Parser;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fragment {
    Ident,
    Tree,
    Meta,
}
enum Pattern {
    Token(Vec<TokenTree>),
    Group(Delimiter, Vec<Pattern>),
    Capture(String, Fragment),
    Repeat(Vec<Pattern>, Option<Vec<TokenTree>>, char),
}
struct Rule {
    pattern: Vec<Pattern>,
    output: TokenStream,
}
#[derive(Clone)]
struct Capture {
    kind: Fragment,
    depth: usize,
    values: BTreeMap<Vec<usize>, TokenStream>,
    lengths: BTreeMap<Vec<usize>, usize>,
}
type Captures = BTreeMap<String, Capture>;
type Definition = Rc<Result<Vec<Rule>, String>>;
type Scope = HashMap<String, Definition>;

fn punctuation(token: &TokenTree, expected: char) -> bool {
    matches!(token, TokenTree::Punct(token) if token.as_char() == expected)
}
fn token_len(tokens: &[TokenTree]) -> usize {
    let Some(TokenTree::Punct(first)) = tokens.first() else {
        return 1;
    };
    if first.spacing() != proc_macro2::Spacing::Joint {
        return 1;
    }
    if first.as_char() == '\'' && matches!(tokens.get(1), Some(TokenTree::Ident(_))) {
        return 2;
    }
    let Some(TokenTree::Punct(second)) = tokens.get(1) else {
        return 1;
    };
    let pair = [first.as_char(), second.as_char()];
    if second.spacing() == proc_macro2::Spacing::Joint
        && let Some(TokenTree::Punct(third)) = tokens.get(2)
        && matches!(
            (pair, third.as_char()),
            (['.', '.'], '.' | '=') | (['<', '<'] | ['>', '>'], '=')
        )
    {
        return 3;
    }
    if matches!(
        pair,
        ['=', '=']
            | ['!', '=']
            | ['<', '=']
            | ['>', '=']
            | ['&', '&']
            | ['|', '|']
            | ['<', '<']
            | ['>', '>']
            | ['+', '=']
            | ['-', '=']
            | ['*', '=']
            | ['/', '=']
            | ['%', '=']
            | ['^', '=']
            | ['&', '=']
            | ['|', '=']
            | ['.', '.']
            | [':', ':']
            | ['-', '>']
            | ['=', '>']
    ) {
        2
    } else {
        1
    }
}
fn equal(left: &[TokenTree], right: &[TokenTree], hygiene: &Hygiene) -> bool {
    left.len() == token_len(right)
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| match (left, right) {
                (TokenTree::Ident(left), TokenTree::Ident(right)) => {
                    hygiene.original(left) == hygiene.original(right)
                }
                (TokenTree::Literal(left), TokenTree::Literal(right)) => {
                    left.to_string() == right.to_string()
                }
                (TokenTree::Punct(left), TokenTree::Punct(right)) => {
                    left.as_char() == right.as_char()
                }
                _ => false,
            })
}
fn repetition(tokens: &[TokenTree]) -> Result<(Option<Vec<TokenTree>>, char, usize), String> {
    let quantifier = |token: &TokenTree| match token {
        TokenTree::Punct(token) if matches!(token.as_char(), '*' | '+' | '?') => {
            Some(token.as_char())
        }
        _ => None,
    };
    if let Some(op) = tokens.first().and_then(quantifier) {
        return Ok((None, op, 1));
    }
    let separator_len = token_len(tokens);
    if let Some(op @ ('*' | '+')) = tokens.get(separator_len).and_then(quantifier) {
        if matches!(tokens.first(), Some(TokenTree::Group(_))) {
            return Err("source macro separator cannot be delimited".into());
        }
        return Ok((
            Some(tokens[..separator_len].to_vec()),
            op,
            separator_len + 1,
        ));
    }
    Err("unsupported source macro repetition".into())
}
fn pattern(tokens: TokenStream) -> Result<Vec<Pattern>, String> {
    let tokens: Vec<_> = tokens.into_iter().collect();
    let mut result = Vec::new();
    let mut at = 0;
    while let Some(token) = tokens.get(at) {
        if punctuation(token, '$') {
            match tokens.get(at + 1) {
                Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Parenthesis => {
                    let (separator, op, consumed) = repetition(&tokens[at + 2..])?;
                    let body = pattern(group.stream())?;
                    if separator.is_none() && nullable(&body) {
                        return Err("source macro repetition matches an empty token tree".into());
                    }
                    result.push(Pattern::Repeat(body, separator, op));
                    at += 2 + consumed;
                    continue;
                }
                Some(TokenTree::Ident(name)) => {
                    let Some(TokenTree::Ident(kind)) = tokens.get(at + 3) else {
                        return Err("unsupported source macro capture".into());
                    };
                    if !tokens
                        .get(at + 2)
                        .is_some_and(|token| punctuation(token, ':'))
                    {
                        return Err("unsupported source macro capture".into());
                    }
                    let kind = match kind.to_string().as_str() {
                        "ident" => Fragment::Ident,
                        "tt" => Fragment::Tree,
                        "meta" => Fragment::Meta,
                        _ => return Err("source macro fragment requires an owned parser".into()),
                    };
                    result.push(Pattern::Capture(name.to_string(), kind));
                    at += 4;
                    continue;
                }
                _ => return Err("unsupported source macro matcher".into()),
            }
        }
        let count = token_len(&tokens[at..]);
        result.push(match token {
            TokenTree::Group(group) => Pattern::Group(group.delimiter(), pattern(group.stream())?),
            _ => Pattern::Token(tokens[at..at + count].to_vec()),
        });
        at += count;
    }
    Ok(result)
}
fn declarations(pattern: &[Pattern], depth: usize, values: &mut Captures) -> Result<(), String> {
    for part in pattern {
        match part {
            Pattern::Capture(name, kind) => {
                if values
                    .insert(
                        name.clone(),
                        Capture {
                            kind: *kind,
                            depth,
                            values: BTreeMap::new(),
                            lengths: BTreeMap::new(),
                        },
                    )
                    .is_some()
                {
                    return Err("duplicate source macro metavariable".into());
                }
            }
            Pattern::Group(_, parts) => declarations(parts, depth, values)?,
            Pattern::Repeat(parts, _, _) => declarations(parts, depth + 1, values)?,
            Pattern::Token(_) => {}
        }
    }
    Ok(())
}
fn rules(tokens: TokenStream) -> Result<Vec<Rule>, String> {
    let parser = |input: syn::parse::ParseStream<'_>| {
        let mut result = Vec::new();
        while !input.is_empty() {
            let matcher: TokenTree = input.parse()?;
            input.parse::<syn::Token![=>]>()?;
            let output: TokenTree = input.parse()?;
            let (TokenTree::Group(matcher), TokenTree::Group(output)) = (matcher, output) else {
                return Err(input.error("macro arms require delimited matcher and output"));
            };
            result.push((matcher.stream(), output.stream()));
            if !input.is_empty() {
                input.parse::<syn::Token![;]>()?;
            }
        }
        Ok(result)
    };
    parser
        .parse2(tokens)
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|(matcher, output)| {
            let pattern = pattern(matcher)?;
            declarations(&pattern, 0, &mut Captures::new())?;
            Ok(Rule { pattern, output })
        })
        .collect()
}

fn nullable(pattern: &[Pattern]) -> bool {
    pattern.iter().all(|part| {
        matches!(part, Pattern::Repeat(body, _, op)
        if *op != '+' || nullable(body))
    })
}
enum Input {
    Leaf(TokenTree),
    Open(Group, usize),
    Close(Delimiter),
}
fn flatten(tokens: TokenStream, result: &mut Vec<Input>) {
    for token in tokens {
        if let TokenTree::Group(group) = token {
            let start = result.len();
            let delimiter = group.delimiter();
            result.push(Input::Open(group.clone(), 0));
            flatten(group.stream(), result);
            let end = result.len();
            result[start] = Input::Open(group, end);
            result.push(Input::Close(delimiter));
        } else {
            result.push(Input::Leaf(token));
        }
    }
}
fn trees(input: &[Input], mut at: usize) -> (Vec<TokenTree>, Vec<usize>) {
    let mut tokens = Vec::new();
    let mut ends = Vec::new();
    while let Some(token) = input.get(at) {
        match token {
            Input::Leaf(token) => {
                tokens.push(token.clone());
                at += 1;
            }
            Input::Open(group, end) => {
                tokens.push(TokenTree::Group(group.clone()));
                at = end + 1;
            }
            Input::Close(_) => break,
        }
        ends.push(at);
    }
    (tokens, ends)
}
fn leaves(input: &[Input]) -> Vec<TokenTree> {
    input
        .iter()
        .take(3)
        .map_while(|token| match token {
            Input::Leaf(token) => Some(token.clone()),
            _ => None,
        })
        .collect()
}
fn fragment_starts(kind: Fragment, token: &Input, spelling: &[TokenTree]) -> bool {
    match kind {
        Fragment::Ident => matches!(token, Input::Leaf(TokenTree::Ident(name)) if name != "_"),
        Fragment::Tree => !matches!(token, Input::Close(_)),
        Fragment::Meta => {
            matches!(token, Input::Leaf(TokenTree::Ident(_)))
                || matches!(token, Input::Open(group, _) if group.delimiter() == Delimiter::None)
                || (token_len(spelling) == 2
                    && spelling
                        .first()
                        .is_some_and(|token| punctuation(token, ':')))
        }
    }
}
fn record_length(parts: &[Pattern], path: &[usize], count: usize, captures: &mut Captures) {
    for part in parts {
        match part {
            Pattern::Capture(name, _) => {
                captures
                    .get_mut(name)
                    .expect("declared capture")
                    .lengths
                    .insert(path.to_vec(), count);
            }
            Pattern::Group(_, body) | Pattern::Repeat(body, _, _) => {
                record_length(body, path, count, captures)
            }
            Pattern::Token(_) => {}
        }
    }
}
#[derive(Clone, Copy)]
enum Task<'a> {
    Part(&'a Pattern),
    Separator(&'a [TokenTree]),
    Close(Delimiter),
    AfterRepeat(&'a [Pattern], Option<&'a [TokenTree]>, char, usize),
}
#[derive(Clone)]
struct State<'a> {
    tasks: Vec<Task<'a>>,
    path: Vec<usize>,
    captures: Captures,
}
fn epsilon<'a>(mut pending: Vec<State<'a>>, budget: &mut usize) -> Result<Vec<State<'a>>, String> {
    let mut ready = Vec::new();
    while let Some(mut state) = pending.pop() {
        if *budget == 0 {
            return Err("source macro matcher state limit".into());
        }
        *budget -= 1;
        match state.tasks.pop() {
            Some(Task::Part(Pattern::Repeat(body, separator, op))) => {
                if *op != '+' {
                    let mut empty = state.clone();
                    record_length(body, &empty.path, 0, &mut empty.captures);
                    pending.push(empty);
                }
                record_length(body, &state.path, 1, &mut state.captures);
                state.path.push(0);
                state
                    .tasks
                    .push(Task::AfterRepeat(body, separator.as_deref(), *op, 1));
                state.tasks.extend(body.iter().rev().map(Task::Part));
                pending.push(state);
            }
            Some(Task::AfterRepeat(body, separator, op, count)) => {
                if op != '?' {
                    let mut repeated = state.clone();
                    *repeated.path.last_mut().expect("repeat index") = count;
                    record_length(
                        body,
                        &repeated.path[..repeated.path.len() - 1],
                        count + 1,
                        &mut repeated.captures,
                    );
                    repeated
                        .tasks
                        .push(Task::AfterRepeat(body, separator, op, count + 1));
                    repeated.tasks.extend(body.iter().rev().map(Task::Part));
                    if let Some(separator) = separator {
                        repeated.tasks.push(Task::Separator(separator));
                    }
                    pending.push(repeated);
                }
                state.path.pop();
                pending.push(state);
            }
            task => {
                state.tasks.extend(task);
                ready.push(state);
            }
        }
    }
    Ok(ready)
}
fn match_pattern(
    pattern: &[Pattern],
    input: &[Input],
    initial: Captures,
    path: &[usize],
    hygiene: &Hygiene,
    budget: &mut usize,
) -> Result<Option<Captures>, String> {
    let mut states = vec![State {
        tasks: pattern.iter().rev().map(Task::Part).collect(),
        path: path.to_vec(),
        captures: initial,
    }];
    let mut at = 0;
    loop {
        let ready = epsilon(states, budget)?;
        let Some(token) = input.get(at) else {
            let mut successes = ready.into_iter().filter(|state| state.tasks.is_empty());
            let success = successes.next();
            if successes.next().is_some() {
                return Err("multiple successful source macro matches".into());
            }
            return Ok(success.map(|state| state.captures));
        };
        let mut fragments = Vec::new();
        let mut literals = Vec::new();
        let spelling = leaves(&input[at..]);
        for state in ready {
            match state.tasks.last().copied() {
                Some(Task::Part(Pattern::Capture(_, kind)))
                    if fragment_starts(*kind, token, &spelling) =>
                {
                    fragments.push(state)
                }
                Some(Task::Part(Pattern::Token(expected)))
                    if !spelling.is_empty() && equal(expected, &spelling, hygiene) =>
                {
                    literals.push(state)
                }
                Some(Task::Separator(expected))
                    if !spelling.is_empty() && equal(expected, &spelling, hygiene) =>
                {
                    literals.push(state)
                }
                Some(Task::Part(Pattern::Group(delimiter, _))) if matches!(token, Input::Open(group, _) if group.delimiter() == *delimiter) => {
                    literals.push(state)
                }
                Some(Task::Close(delimiter)) if matches!(token, Input::Close(actual) if *actual == delimiter) => {
                    literals.push(state)
                }
                _ => {}
            }
        }
        if fragments.len() > 1 || (!fragments.is_empty() && !literals.is_empty()) {
            return Err("local ambiguity in source macro matcher".into());
        }
        if let Some(mut state) = fragments.pop() {
            let Some(Task::Part(Pattern::Capture(name, kind))) = state.tasks.pop() else {
                unreachable!()
            };
            let (remaining, ends) = trees(input, at);
            let count = match kind {
                Fragment::Ident => 1,
                Fragment::Tree => {
                    if matches!(token, Input::Open(group, _) if group.delimiter() == Delimiter::None)
                    {
                        return Err(
                            "source macro cannot reinterpret an opaque fragment as tt".into()
                        );
                    }
                    token_len(&remaining)
                }
                Fragment::Meta => {
                    let parser = |input: syn::parse::ParseStream<'_>| {
                        input.parse::<syn::Meta>()?;
                        input.parse::<TokenStream>()
                    };
                    let rest = parser
                        .parse2(remaining.iter().cloned().collect())
                        .map_err(|error| format!("source macro meta fragment: {error}"))?;
                    remaining.len() - rest.into_iter().count()
                }
            };
            state
                .captures
                .get_mut(name)
                .expect("declared capture")
                .values
                .insert(
                    state.path.clone(),
                    remaining[..count].iter().cloned().collect(),
                );
            at = ends[count - 1];
            states = vec![state];
        } else {
            states = Vec::new();
            for mut state in literals {
                if let Some(Task::Part(Pattern::Group(delimiter, body))) = state.tasks.pop() {
                    state.tasks.push(Task::Close(*delimiter));
                    state.tasks.extend(body.iter().rev().map(Task::Part));
                }
                states.push(state);
            }
            if states.is_empty() {
                return Ok(None);
            }
            at += if spelling.is_empty() {
                1
            } else {
                token_len(&spelling)
            };
        }
    }
}
fn repetition_count(
    tokens: TokenStream,
    captures: &Captures,
    path: &[usize],
) -> Result<Option<usize>, String> {
    let tokens: Vec<_> = tokens.into_iter().collect();
    let mut count = None;
    for (at, token) in tokens.iter().enumerate() {
        if let TokenTree::Group(group) = token {
            if let Some(nested) = repetition_count(group.stream(), captures, path)? {
                if count.is_some_and(|previous| previous != nested) {
                    return Err("source macro repetition capture lengths disagree".into());
                }
                count = Some(nested);
            }
        }
        if punctuation(token, '$')
            && let Some(TokenTree::Ident(name)) = tokens.get(at + 1)
            && let Some(capture) = captures.get(&name.to_string())
            && capture.depth > path.len()
        {
            let length = capture.lengths.get(path).copied().unwrap_or(0);
            if count.is_some_and(|previous| previous != length) {
                return Err("source macro repetition capture lengths disagree".into());
            }
            count = Some(length);
        }
    }
    Ok(count)
}
fn transcribe(
    tokens: TokenStream,
    captures: &Captures,
    path: &mut Vec<usize>,
    hygiene: &mut Hygiene,
    context: usize,
) -> Result<TokenStream, String> {
    let tokens: Vec<_> = tokens.into_iter().collect();
    let mut output = TokenStream::new();
    let mut at = 0;
    while let Some(token) = tokens.get(at) {
        if punctuation(token, '$') {
            match tokens.get(at + 1) {
                Some(TokenTree::Ident(name)) => {
                    if name == "crate" {
                        output.extend([TokenTree::Ident(name.clone())]);
                        at += 2;
                        continue;
                    }
                    let capture = captures
                        .get(&name.to_string())
                        .ok_or("unknown source macro metavariable")?;
                    if path.len() < capture.depth {
                        return Err("source macro capture is still repeating".into());
                    }
                    let value = capture
                        .values
                        .get(&path[..capture.depth])
                        .ok_or("missing source macro capture")?;
                    if capture.kind == Fragment::Meta {
                        // Invisible groups preserve opaque AST forwarding through nested calls.
                        output
                            .extend([TokenTree::Group(Group::new(Delimiter::None, value.clone()))]);
                    } else {
                        output.extend(value.clone());
                    }
                    at += 2;
                    continue;
                }
                Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Parenthesis => {
                    let (separator, op, consumed) = repetition(&tokens[at + 2..])?;
                    let count = repetition_count(group.stream(), captures, path)?
                        .ok_or("source macro repetition has no capture at this depth")?;
                    if (op == '+' && count == 0) || (op == '?' && count > 1) {
                        return Err("source macro repetition has incompatible cardinality".into());
                    }
                    for index in 0..count {
                        if index > 0
                            && let Some(separator) = &separator
                        {
                            output.extend(separator.iter().cloned());
                        }
                        path.push(index);
                        output.extend(transcribe(
                            group.stream(),
                            captures,
                            path,
                            hygiene,
                            context,
                        )?);
                        path.pop();
                    }
                    at += 2 + consumed;
                    continue;
                }
                _ => return Err("unsupported source macro transcription".into()),
            }
        }
        output.extend([match token {
            TokenTree::Ident(name) => TokenTree::Ident(hygiene.literal(name, context)?),
            TokenTree::Literal(value) => {
                let mut value = value.clone();
                value.set_span(hygiene.context_span(context)?);
                TokenTree::Literal(value)
            }
            TokenTree::Group(group) => TokenTree::Group(Group::new(
                group.delimiter(),
                transcribe(group.stream(), captures, path, hygiene, context)?,
            )),
            token => token.clone(),
        }]);
        at += 1;
    }
    Ok(output)
}
fn invoke(
    definition: &[Rule],
    input: TokenStream,
    hygiene: &mut Hygiene,
) -> Result<Vec<syn::Item>, String> {
    let mut tokens = Vec::new();
    flatten(input, &mut tokens);
    for rule in definition {
        let mut captures = Captures::new();
        declarations(&rule.pattern, 0, &mut captures)?;
        if let Some(captures) =
            match_pattern(&rule.pattern, &tokens, captures, &[], hygiene, &mut 100_000)?
        {
            let context = hygiene.next;
            hygiene.next += 1;
            let output = transcribe(
                rule.output.clone(),
                &captures,
                &mut Vec::new(),
                hygiene,
                context,
            )?;
            return syn::parse2::<syn::File>(output)
                .map(|file| file.items)
                .map_err(|error| format!("source macro output is not owned Rust items: {error}"));
        }
    }
    Err("no source macro rule matches invocation".into())
}
fn items(
    items: &mut Vec<syn::Item>,
    inherited: &Scope,
    active: bool,
    budget: &mut usize,
    hygiene: &mut Hygiene,
) -> Result<Scope, String> {
    let mut scope = inherited.clone();
    let mut exports = Scope::new();
    let mut pending: VecDeque<_> = std::mem::take(items).into();
    while let Some(mut item) = pending.pop_front() {
        match &mut item {
            syn::Item::Macro(item) => hygiene.attributes(&mut item.attrs),
            syn::Item::Mod(item) => hygiene.attributes(&mut item.attrs),
            _ => {}
        }
        let presence = item_presence(&item);
        if presence == Presence::Inactive {
            items.push(item);
            continue;
        }
        match &mut item {
            syn::Item::Macro(item) if item.mac.path.is_ident("macro_rules") => {
                if let Some(name) = &item.ident {
                    let definition = Rc::new(if active && presence == Presence::Active {
                        rules(item.mac.tokens.clone())
                    } else {
                        Err("source macro definition depends on unresolved cfg".into())
                    });
                    scope.insert(hygiene.original(name).to_string(), definition.clone());
                    exports.insert(hygiene.original(name).to_string(), definition);
                }
            }
            syn::Item::Macro(item) if active && presence == Presence::Active => {
                if let Some(name) = item.mac.path.get_ident()
                    && let Some(definition) = scope.get(&hygiene.original(name).to_string())
                {
                    if *budget == 0 {
                        return Err("source item macro expansion limit".into());
                    }
                    *budget -= 1;
                    let definition = definition
                        .as_ref()
                        .as_ref()
                        .map_err(|error| format!("source macro `{name}`: {error}"))?;
                    let output = invoke(definition, item.mac.tokens.clone(), hygiene)
                        .map_err(|error| format!("source macro `{name}`: {error}"))?;
                    for item in output.into_iter().rev() {
                        pending.push_front(item);
                    }
                    continue;
                }
                if item
                    .mac
                    .path
                    .get_ident()
                    .is_some_and(|name| hygiene.original(name) == "compile_error")
                {
                    return Err(format!("source compile_error!: {}", item.mac.tokens));
                }
            }
            syn::Item::Mod(module) => {
                if let Some((_, nested)) = &mut module.content {
                    let definitions = self::items(
                        nested,
                        &scope,
                        active && presence == Presence::Active,
                        budget,
                        hygiene,
                    )?;
                    if module
                        .attrs
                        .iter()
                        .any(|attr| attr.path().is_ident("macro_use"))
                    {
                        scope.extend(definitions.clone());
                        exports.extend(definitions);
                    }
                }
            }
            _ => {}
        }
        items.push(item);
    }
    Ok(exports)
}
pub(super) fn expand(file: &mut syn::File) -> Result<(), String> {
    let source = file.to_token_stream();
    let mut hygiene = Hygiene::new(&source.to_string(), source);
    items(
        &mut file.items,
        &Scope::new(),
        attrs_presence(&file.attrs) == Presence::Active,
        &mut 4096,
        &mut hygiene,
    )?;
    if hygiene.next > 1 {
        hygiene.normalize(file)?;
    }
    Ok(())
}
