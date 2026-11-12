#!/usr/bin/env python3
"""Generate and audit the validator's observation contracts with a fail-closed Rust syntax recognizer.

The audit checks declared collection consumption, typed-accessor bypasses and register drift.
It deliberately does not certify the semantic truth of a declaration; fixtures do that.
"""
from pathlib import Path
import argparse, atexit, json, re, subprocess, sys
from collections import Counter

FIELDS = {
    'comment_defs','comment_refs','comment_para_ids','docpr','bookmarks','revisions',
    'footnote_refs','endnote_refs','footnote_defs','endnote_defs','header_refs','footer_refs','style_refs','num_ids',
    'listings','special_notes','content_parts','content_part_exemptions','ink_content_parts',
    'refs','entries','rels','content_types', 'note_listings',
}
BASE_FIELDS = frozenset(FIELDS)
ACCESSORS = {'prefix','is_complete','project','into_complete_set','missing','append','retain_budget','push_bounded','invalidate'}
# Input here is the recognizer's canonical Rust token stream, with literals masked.
# Non-ASCII token characters are identifier characters; Python's \w omits Rust XID
# continuations (combining marks) and some XID starts. Never split such an identifier.
IDENT_CHAR = r'[\w\x80-\U0010ffff]'
IDENT = IDENT_CHAR + '+'
IDENT_START = r'(?<![\w\x80-\U0010ffff])'
IDENT_END = r'(?![\w\x80-\U0010ffff])'
MEMBERSHIP = IDENT_START + r'((?:' + IDENT + r'\s*\.\s*)*' + IDENT + r')\s*\.\s*contains(?:_key)?\s*\('



def mask(text):
    """Blank Rust comments and literals, preserving all byte offsets and newlines."""
    pattern = r'//[^\n]*|/\*|r(#+)?"[\s\S]*?"\1|"(?:\\.|[^"\\])*"|\'(?:\\.|[^\'\\])\''
    # Raw strings need a backreference with an empty group rather than an unmatched optional one.
    pattern = pattern.replace('(#+)?', '(#*)')
    # Rust block comments nest. Stopping at the first */ leaves comment tokens
    # visible, where a semicolon or pipe could split a closure-parameter candidate.
    pieces = []; at = 0
    token = re.compile(pattern)
    while match := token.search(text, at):
        end = match.end()
        if match.group() == '/*':
            depth = 1
            end = len(text)
            for delimiter in re.finditer(r'/\*|\*/', text[match.end():]):
                depth += 1 if delimiter.group() == '/*' else -1
                if depth == 0:
                    end = match.end() + delimiter.end()
                    break
        pieces.extend((text[at:match.start()], re.sub(r'[^\n]', ' ', text[match.start():end])))
        at = end
    return ''.join(pieces) + text[at:]


def close_brace(code, opening):
    depth = 1
    for index in range(opening + 1, len(code)):
        depth += (code[index] == '{') - (code[index] == '}')
        if depth == 0:
            return index
    raise ValueError('unbalanced Rust braces in contract audit')


def production(text):
    code = mask(text)
    # Exclude inline test modules, but not #[cfg(test)] external mod declarations at the crate root.
    for match in list(re.finditer(r'#\[cfg\(test\)\]\s*(?:#\[[^\]]*\]\s*)*mod\s+\w+\s*\{', code)):
        end = close_brace(code, match.end()-1)+1
        text = text[:match.start()] + re.sub(r'[^\n]', ' ', text[match.start():end]) + text[end:]
    return text


def normalized(code):
    """Raw Rust identifiers have the same audit identity as ordinary identifiers."""
    return re.sub(r'\br#(?=[^\W\d])', '', code)


_RECOGNIZER = None


def recognize(sources):
    """Parse every audited token; an unsupported body or grammar node is an error.

    The helper is a standalone, locked build-time tool, never a validator dependency.
    It stays alive for in-memory self-tests so every mutation uses the real parser.
    """
    global _RECOGNIZER
    if _RECOGNIZER is None:
        root = Path(__file__).resolve().parents[3]
        manifest = Path(__file__).parent / 'recognizer/Cargo.toml'
        target = root / 'target/observation-recognizer'
        subprocess.run(['cargo', 'build', '--locked', '--quiet', '--manifest-path', str(manifest),
                        '--target-dir', str(target)], check=True)
        _RECOGNIZER = subprocess.Popen([str(target / 'debug/observation-recognizer')],
                                      stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        atexit.register(_RECOGNIZER.terminate)
    _RECOGNIZER.stdin.write(json.dumps(sources) + '\n')
    _RECOGNIZER.stdin.flush()
    line = _RECOGNIZER.stdout.readline()
    if not line:
        raise ValueError('syntax recognizer terminated without accepting the sources')
    response = json.loads(line)
    if 'error' in response:
        raise ValueError(response['error'])
    return response['ok']


def consumption(code, enum_variants=()):
    # Qualified enum values are expressions, not collection types. Infer these
    # identities from recognized enum definitions instead of naming source exceptions.
    type_code = code
    for variant in enum_variants:
        if variant.rsplit('::', 1)[-1] not in {'Observed','CompleteSet','Vec','HashSet','HashMap','BTreeSet','BTreeMap'}:
            continue
        path = r'\s*::\s*'.join(map(re.escape, variant.split('::')))
        type_code = re.sub(IDENT_START + path + IDENT_END, '', type_code)
    fields = ['field.'+m.group(1) for m in re.finditer(r'\.\s*(' + IDENT + r')',code) if m.group(1) in FIELDS]
    methods = ['access.'+m.group(1) for m in re.finditer(r'\.\s*(' + IDENT + r')\s*(?:\(|::)',code) if m.group(1) in ACCESSORS]
    parsers = ['parser.'+m.group(1) for m in re.finditer(IDENT_START + r'(parse_' + IDENT + r')\s*\(',code)]
    types = ['type.'+m.group() for m in re.finditer(IDENT_START + r'(?:Observed|CompleteSet|Vec|HashSet|HashMap|BTreeSet|BTreeMap)' + IDENT_END, type_code)]
    return [f'{name}*{count}' for name,count in sorted(Counter(fields + methods + parsers + types).items())]


def representation_footprint(header, code):
    private = Counter(re.findall(r'\bobserved_(?:value|status)\b', code))
    membership = Counter(re.sub(r'\s+', '', m.group(1)) for m in re.finditer(MEMBERSHIP, code))
    return sorted(consumption(header+code) + [f'repr.{k}*{v}' for k,v in sorted(private.items())]
                  + [f'membership.{k}*{v}' for k,v in sorted(membership.items())])


def discover_fields(sources):
    for name,text in sources.items():
        if name in {"model.rs", "reserved.rs", "contracts_generated.rs"} or name.endswith("_tests.rs"): continue
        code=normalized(mask(production(text)))
        FIELDS.update(re.findall(r'^[ \t]*(?:pub(?:\(crate\))?[ \t]+)?(\w+)[ \t]*:[^\n]*(?:Observed|Vec|HashSet|HashMap|BTreeSet|BTreeMap)',code,re.M))



def rule_registry(sources):
    return set(re.findall(r'RuleSpec\s*\{\s*id:\s*"([A-Z0-9-]+)"', sources['lib.rs']))


def declarations(sources):
    rows=[];errors=[]
    providers = json.loads((Path(__file__).parent / 'representation-contracts.json').read_text())
    seen_providers = set()
    FIELDS.clear(); FIELDS.update(BASE_FIELDS)
    discover_fields(sources)
    known_rules=rule_registry(sources)
    audited = {name: text for name, text in sources.items()
               if not name.endswith('_tests.rs') and name not in {'model.rs','reserved.rs','contracts_generated.rs'}}
    recognized = recognize(audited)
    FIELDS.update(f for file in recognized.values() for f in file['collection_fields'])
    enum_variants = [v for file in recognized.values() for v in file['enum_variants']]
    for name,text in sorted(sources.items()):
        if name.endswith('_tests.rs') or name in {'model.rs','reserved.rs','contracts_generated.rs'}:
            continue
        for module in recognized[name]['modules']:
            if module + '.rs' not in sources:
                errors.append(f'{name}: external module {module} has no supplied audited source or explicit excluded table/test/policy source')
        surface = normalized(mask(' '.join(recognized[name]['surface'])))
        if re.search(r'\bPackageRegistryContext\b|(?:\.|::)\s*contexts\b', surface):
            errors.append(f'{name}: module-scope package-registry aliases/queries are unsupported')
        # A template and an invocation may yield identical units. Audit all instances;
        # emit a single row only when the entire normalized header/body is identical.
        seen = set()
        for unit in recognized[name]['units']:
            func, header, body = unit['name'], unit['header'], unit['body']
            header = normalized(mask(header)); code = normalized(mask(body))
            uses = consumption(header+code)
            bounded_uses = consumption(header+code, enum_variants)
            decl = re.search(r'observation_contract\s*!\s*\{', code)
            declaration = body[decl.end():close_brace(code, decl.end()-1)] if decl else ''
            if name != 'observed.rs' and re.search(r'\.\s*(?:observed_value|observed_status)\b',code):
                errors.append(f'{name}::{func}: bypasses private observation representation')
            if 'PackageRegistryContext' in header+code or re.search(r'(?:\.|::)\s*contexts\b',code):
                if (name,func) not in {('lib.rs','report_unreferenced_comments'),('lib.rs','check_custom_xml'),('ownership.rs','contexts')}:
                    errors.append(f'{name}::{func}: undeclared package-registry exception')
            if re.search(r'\.\s*difference\s*\(', code):
                errors.append(f'{name}::{func}: raw absence difference bypasses CompleteSet')
            if name == 'observed.rs':
                # The private implementation has explicit build-time contracts too.
                # No function, initializer or consuming body can escape by file name.
                footprint = representation_footprint(header, code)
                if providers.get(func) != dict(header=header, collections=footprint) or func in seen_providers:
                    errors.append(f'{name}::{func}: representation contract differs or is missing')
                seen_providers.add(func)
                rows.append(dict(helper=f'{name}::{func}', rules='-', collections=','.join(footprint) or '-',
                    scope='private Observed/CompleteSet representation provider; signature and footprint pinned in observations/representation-contracts.json',
                    loss='local completeness and allocation budgets carried by the typed representation',
                    fallback='retain prefix; incomplete observations cannot construct CompleteSet; fixtures check semantics',
                    package_loss='independent'))
                continue
            # Raw definition membership must not bypass CompleteSet, even after copying a prefix.
            # These receivers are structural metadata, lexical validation, or allocation guards.
            structural_membership = {
                ('lib.rs', 'absence_suppressed', 'PACKAGE_ABSENCE_RULES'),
                ('lib.rs', 'check_note_pr', 'reverse_ok'),
                ('lib.rs', 'report_malformed', 'pkg.reachable'),
                ('lib.rs', 'check_typed_content', 'non_main_owner_rels'),
                ('lib.rs', 'report_unreferenced_comments', 'incomplete_owners'),
                ('lib.rs', 'supporting_part', 'acc.coverage'),
                ('lib.rs', 'check_custom_xml', 'custom_xml_owners'),
                ('lib.rs', 'check_media', 'image_targets'),
                ('scan.rs', 'check_protection', 'k'),
                ('scan.rs', 'check_para_ids', 'state.para_ids'),
                ('scan.rs', 'attr_plain', 'k'),
                ('package.rs', 'is_main_content_type', 'MAIN_CONTENT_TYPES'),
                ('package.rs', 'load', 'xml_parts'),
                ('package.rs', 'classified_xml_parts', 'role_targets'),
                ('package.rs', 'is_xml_part', 'pkg.xml_parts'),
                ('ownership.rs', 'with_budget', 'roots'),
                ('ownership.rs', 'parts', 'owners'),
            }
            for access in re.finditer(MEMBERSHIP, code):
                receiver=re.sub(r'\s+', '', access.group(1))
                if (name,func,receiver) not in structural_membership:
                    errors.append(f'{name}::{func}: raw membership {receiver} bypasses CompleteSet or undeclared structural scope')
            if not bounded_uses and not decl and not re.search(r'\bacc\s*\.\s*(?:add|add_graded|ran)\s*\(', code):
                continue
            if not decl:
                errors.append(f'{name}::{func}: bounded consumption has no observation contract: {uses}')
                continue
            data=dict(re.findall(r'(\w+)\s*:\s*"((?:\\.|[^"\\])*)"\s*;',declaration))
            required={'rules','collections','scope','loss','fallback','package_loss'}
            if set(data)!=required or any(not data.get(k,'').strip() for k in required):
                errors.append(f'{name}::{func}: missing contract field/fallback')
                continue
            declared=[] if data['collections']=='-' else sorted(data['collections'].split(','))
            if declared!=uses:
                errors.append(f'{name}::{func}: declared collections differ from bounded accesses: declared={declared}, used={uses}')
            declared_rules=set(data['rules'].split(',')) - {'-'}
            called_rules=set(re.findall(r'"([A-Z0-9-]+)"',body)) & known_rules
            if not called_rules <= declared_rules:
                errors.append(f'{name}::{func}: rule consumption omitted: {called_rules-declared_rules}')
            if not declared_rules <= known_rules:
                errors.append(f'{name}::{func}: unknown rule IDs: {declared_rules-known_rules}')
            if data['package_loss'] != 'independent' and not set(data['package_loss'].split(',')) <= declared_rules:
                errors.append(f'{name}::{func}: undeclared unknown-package-identity rule policy')
            key = (func, header, code, tuple(sorted(data.items())))
            if key not in seen:
                rows.append(dict(helper=f'{name}::{func}', **data))
                seen.add(key)
    if seen_providers != set(providers): errors.append('representation contracts missing providers')
    covered={rule for row in rows for rule in row['rules'].split(',')}
    if known_rules-covered: errors.append(f'rule registry lacks observation contracts: {sorted(known_rules-covered)}')
    if errors:
        raise ValueError('\n'.join(errors))
    return rows


def outputs(rows):
    header = ('# Validator observation contracts\n\n'
        'Generated by `python3 crates/validate/observations/generate.py`; do not edit.\n'
        'Run with `--check` to audit consumers and drift; `--self-test` exercises rejection paths.\n\n'
        'The gate enforces fail-closed syntax recognition over the production Rust in `src/lib.rs`, `src/observed.rs`, `src/ownership.rs`, `src/package.rs`, `src/resolution.rs`, and `src/scan.rs`. Inline `#[cfg(test)]` modules, separate test files, generated policy, and the non-consuming `model.rs`/`reserved.rs` tables are outside this boundary.\n'
        'Every item, expression body and pattern in those files must match the closed grammar in `observations/recognizer/src/main.rs`; unsupported syntax fails. External module declarations must resolve to a supplied source file or an explicit table/test/policy exclusion. Functions and impl methods, nested closures, const/static initializers, discriminants and type-level expressions are audited. Named-field patterns in binding, parameter, closure, match and assignment positions, pattern macros, traits and unaudited item/body kinds are rejected. Raw identifiers are normalized before footprint and registry checks.\n'
        'Single-arm item macro templates with ident/literal fragments and no repetition are instantiated and audited, both at definition and invocation; all other definitions and item macros fail. `observation_contract!` must expand to no code. The recognized `format!`, `vec!`, `matches!` and `debug_assert!` argument grammars are parsed and traversed, including their patterns and nested bodies; other expression macros and local definitions/imports shadowing these standard macros and glob imports fail. Named collection fields are discovered from recognized expansions too. Footprint identifiers retain complete Unicode identifiers, including combining marks; Python word boundaries are not Rust identifier boundaries.\n'
        'Exact declared lexical footprints cover structural adapters and collection-typed locals; enum values alone do not trigger a collection declaration. Private representation providers have explicit signature/footprint contracts in `observations/representation-contracts.json`, with no file-wide function exemption. Macro instances with identical normalized bodies/headers/contracts share a register row. Fixtures check semantic truth; this is not Rust name resolution or an alias/dataflow proof.\n'
        'Build scripts, compiler expansion and procedural-macro implementations are outside the audit: recognized built-in derives and the existing serde `Serialize` derive are accepted by attribute name, without auditing their generated code. Other attribute macros and `cfg_attr` fail. Rust privacy seals the observation representation. The standalone locked recognizer is build-time tooling and does not enter validator/WASM dependencies.\n'
        'Only `check_custom_xml` (CUSTOMXML-SCHEMA) and `report_unreferenced_comments` (informational coverage) may consult PackageRegistryContext.\n'
        'That context admits established-unowned declarations to the primary package registry; it never grants story ownership.\n'
        'Duplicate relationship IDs bind the first XML declaration in every reference path; swapping duplicates changes binding evidence.\n\n'
        '| Helper | Rules/conclusion | Collections/accessors | Intentional scope | Incompleteness causes | Fallback | Unknown package identity |\n'
        '|---|---|---|---|---|---|---|\n')
    md=header+''.join('| '+' | '.join(row[k].replace('|','/') for k in ['helper','rules','collections','scope','loss','fallback','package_loss'])+' |\n' for row in rows)
    rules=sorted({r for row in rows if row['package_loss']!='independent' for r in row['package_loss'].split(',')})
    rust='//! Generated observation-contract policy. Do not edit; see observations/generate.py.\n'
    rust+='/// Rules whose failing absence conclusions cannot survive an unknown package identity.\n'
    rust+='pub(crate) const PACKAGE_ABSENCE_RULES: &[&str] = &[\n'+''.join(f'    "{r}",\n' for r in rules)+'];\n'
    return md,rust


def self_test(sources, rows):
    # Mutate in memory only. Neither repository source nor generated output is rewritten.
    mutations = {
      'undeclared consumer': ('lib.rs', lambda s:s+'\nfn invented_check(value: Observed<Vec<u8>>) { let _ = value.prefix(); }\n'),
      'undeclared array consumer': ('lib.rs', lambda s:s+'\nfn invented_array(v: &[Observed<Vec<u8>>; 2]) { let _ = v[0].prefix(); }\n'),
      'missing fallback': ('lib.rs', lambda s:s.replace('fallback: \"', 'removed_fallback: \"',1)),
      'accessor bypass': ('lib.rs', lambda s:s+'\nfn invented_bypass(value: Observed<Vec<u8>>) { let _ = value.observed_value; }\n'),
      'exception leakage': ('lib.rs', lambda s:s+'\nfn invented_registry() { let _ = ownership::PackageRegistryContext::new(graph); }\n'),
      'new undeclared accessor': ('lib.rs',lambda s:s.replace('acc.ran("CUSTOMXML-SCHEMA");','acc.ran("CUSTOMXML-SCHEMA"); let _ = new_collection.prefix();',1)),
      'raw membership bypass': ('lib.rs',lambda s:s.replace('acc.ran("CUSTOMXML-SCHEMA");','acc.ran("CUSTOMXML-SCHEMA"); let absent = !raw.contains(&1);',1)),
      'raw absence difference': ('lib.rs',lambda s:s+'\nfn invented_absence() { let _ = a.difference(b); }\n'),
      'destructured package read': ('lib.rs',lambda s:s+'\nfn unregistered_read(pkg: &Package) -> usize { let Package { entries: items, .. } = pkg; items.len() }\n'),
      'module registry alias and method pointer': ('lib.rs',lambda s:s+'''\nuse ownership::PackageRegistryContext as HiddenRegistry;
const REGISTRY_QUERY: for<'a> fn(&'a HiddenRegistry, &str) -> &'a [OwnerId] = HiddenRegistry::contexts;
fn unregistered_registry(graph: &PartGraph) -> bool {
    let registry = HiddenRegistry::new(graph);
    REGISTRY_QUERY(&registry, "x").is_empty()
}
'''),
      'destructured parameter read': ('lib.rs',lambda s:s+'\nfn unregistered_parameter(Package { entries: items, .. }: &Package) -> usize { items.len() }\n'),
      'destructured match read': ('lib.rs',lambda s:s+'\nfn unregistered_match(pkg: &Package) -> usize { match pkg { Package { entries: items, .. } => items.len() } }\n'),
      'destructured for read': ('lib.rs',lambda s:s+'\nfn unregistered_for(packages: &[Package]) { for Package { entries: items, .. } in packages { use_items(items); } }\n'),
      'immediately invoked destructured closure': ('lib.rs',lambda s:s+'\nfn unregistered_read(pkg: &Package) -> usize { (|Package { entries: items, .. }: &Package| items.len())(pkg) }\n'),
      'let-bound destructured closure': ('lib.rs',lambda s:s+'\nfn unregistered_read(pkg: &Package) -> usize { let count = |Package { entries: items, .. }: &Package| items.len(); count(pkg) }\n'),
      'const fn-pointer destructured closure': ('lib.rs',lambda s:s+'\nconst UNREGISTERED_READ: fn(&Package) -> usize = |Package { entries: items, .. }| items.len();\n'),
      'closure argument with nested pattern': ('lib.rs',lambda s:s+'\nfn unregistered_read() { consume(move |(_, [Package { entries: items, .. }]): ((), [&Package; 1])| items.len()); }\n'),
      'returned closure with later parameter': ('lib.rs',lambda s:s+'\nfn unregistered_read() -> impl Fn((), &Package) -> usize { move |_, &Package { ref entries, .. }| entries.len() }\n'),
      'static fn-pointer destructured closure': ('lib.rs',lambda s:s+'\nstatic UNREGISTERED_READ: fn(&Package) -> usize = |crate::package::Package { entries: items, .. }| items.len();\n'),
      'array-element destructured closure': ('lib.rs',lambda s:s+'\nfn unregistered_read() { consume([|whole @ Package { entries: _, .. }: &Package| use_items(whole)]); }\n'),
      'async destructured closure': ('lib.rs',lambda s:s+'\nfn unregistered_read() { consume(async move |Package { entries, .. }: &Package| entries.len()); }\n'),
      'nested struct closure pattern': ('lib.rs',lambda s:s+'\nfn unregistered_read() { consume(|Wrapper { inner: Package { entries: items, .. } }| items.len()); }\n'),
      'closure pattern after associated-type constraint': ('lib.rs',lambda s:s+'\nfn unregistered_read() { consume(|_: &dyn Trait<Item = ()>, Package { entries, .. }: &Package| entries.len()); }\n'),
      'closure pattern with attributes and comments': ('lib.rs',lambda s:s+'\nfn unregistered_read() { consume(|#[allow(unused)] /* pattern */ (Package { entries, .. },): (&Package,)| entries.len()); }\n'),
      'destructured closure returning adjacent closure': ('lib.rs',lambda s:s+'\nfn unregistered_read() { consume(|Package { entries, .. }: &Package||()| entries.len()); }\n'),
      'adjacent inner destructured closure': ('lib.rs',lambda s:s+'\nfn unregistered_read() { consume(|()||Package { entries, .. }: &Package| entries.len()); }\n'),
      'closure with parenthesized or-pattern': ('lib.rs',lambda s:s+'\nfn unregistered_read() { consume(|(Package { entries, .. } | Package { entries, .. })| entries.len()); }\n'),
      'closure with generic struct path': ('lib.rs',lambda s:s+'\nfn unregistered_read() { consume(|Package::<Vec<u8>> { entries, .. }| entries.len()); }\n'),
      'closure with qualified struct path': ('lib.rs',lambda s:s+'\nfn unregistered_read() { consume(|<Container as Trait>::Package { entries, .. }| entries.len()); }\n'),
      'closure with Unicode identifier continuation': ('lib.rs',lambda s:s+'\ntype Package\u0301 = Package;\nfn unregistered_read() { consume(|Package\u0301 { entries, .. }| entries.len()); }\n'),
      'closure with nested block comment': ('lib.rs',lambda s:s+'\nfn unregistered_read() { consume(|Package /* outer /* inner */ ; */ { entries, .. }| entries.len()); }\n'),
      'module registry type alias': ('lib.rs',lambda s:s+'\ntype HiddenRegistry = ownership::PackageRegistryContext;\n'),
      'module registry method pointer': ('lib.rs',lambda s:s+"\nconst QUERY: for<'a> fn(&'a ownership::PackageRegistryContext, &str) -> &'a [OwnerId] = ownership::PackageRegistryContext::contexts;\n"),
    }
    mutations.update({
      'delta16 destructuring assignment': ('lib.rs', lambda s:s+'\nfn unregistered_read(pkg: Package) -> usize { let items; Package { entries: items, .. } = pkg; items.len() }'),
      'delta16 parenthesized destructuring assignment': ('lib.rs', lambda s:s+'\nfn unregistered_read(pkg: Package) -> usize { let items; (Package { entries: items, .. }) = pkg; items.len() }'),
      'delta16 macro-produced closure pattern': ('lib.rs', lambda s:s+'''
macro_rules! package_pattern { ($items:ident) => { Package { entries: $items, .. } }; }
fn unregistered_read(pkg: &Package) -> usize { (|package_pattern!(items): &Package| items.len())(pkg) }
'''),
      'delta16 raw field read': ('lib.rs', lambda s:s+'\nfn unregistered_read(pkg: &Package) -> usize { pkg.r#entries.len() }'),
      'delta16 raw registry method': ('ownership.rs', lambda s:s.replace('impl PackageRegistryContext {', 'impl PackageRegistryContext { fn unregistered_registry(&self) -> usize { self.r#contexts("x").len() }', 1)),
      'delta16 module callback body': ('lib.rs', lambda s:s+'\nconst UNREGISTERED_READ: fn(&Package) -> usize = |pkg| pkg.entries.len();'),
      'static callback body': ('lib.rs', lambda s:s+'\nstatic UNREGISTERED_READ: fn(&Package) -> usize = |pkg| (|p: &Package| p.entries.len())(pkg);'),
      'raw private representation': ('lib.rs', lambda s:s+'\nfn invented(value: u8) { let _ = value.r#observed_value; }'),
      'raw accessor': ('lib.rs', lambda s:s+'\nfn invented(value: u8) { let _ = value.r#prefix(); }'),
      'raw registry type alias': ('lib.rs', lambda s:s+'\ntype Hidden = ownership::r#PackageRegistryContext;'),
      'raw registry function name': ('ownership.rs', lambda s:s.replace('impl PackageRegistryContext {', 'impl PackageRegistryContext { fn r#unregistered_registry(&self) -> usize { self.contexts("x").len() }', 1)),
      'registry in representation provider': ('observed.rs', lambda s:s+'\nfn hidden() { let _ = x.r#contexts("x"); }'),
      'unsupported union item': ('lib.rs', lambda s:s+'\nunion Unsupported { field: usize }'),
      'unsupported trait body': ('lib.rs', lambda s:s+'\ntrait Hidden { fn read(pkg: &Package) -> usize { pkg.entries.len() } }'),
      'unsupported foreign item': ('lib.rs', lambda s:s+'\nunsafe extern "C" { fn hidden(); }'),
      'unsupported production module': ('lib.rs', lambda s:s+'\nmod hidden { fn read(pkg: &super::Package) -> usize { pkg.entries.len() } }'),
      'unsupported attribute macro': ('lib.rs', lambda s:s+'\n#[hidden] fn read() {}'),
      'unsupported pattern macro in binding': ('lib.rs', lambda s:s+'\nfn read(pkg: &Package) { let hidden!(items) = pkg; }'),
      'unsupported pattern macro in parameter': ('lib.rs', lambda s:s+'\nfn read(hidden!(items): &Package) {}'),
      'unsupported pattern macro in match': ('lib.rs', lambda s:s+'\nfn read(pkg: &Package) { match pkg { hidden!(items) => {} } }'),
      'unsupported pattern macro in assignment': ('lib.rs', lambda s:s+'\nfn read(pkg: Package) { hidden!(items) = pkg; }'),
      'unsupported expression macro': ('lib.rs', lambda s:s+'\nfn read(pkg: &Package) { hidden!(pkg.entries); }'),
      'unsupported macro repetition': ('lib.rs', lambda s:s+'\nmacro_rules! hidden { ($($x:ident),*) => { $($x)* }; }'),
      'unsupported async body': ('lib.rs', lambda s:s+'\nfn read(pkg: &Package) { let _ = async { pkg.entries.len() }; }'),
      'nested closure body': ('lib.rs', lambda s:s+'\nfn read(pkg: &Package) { consume(|| (|p: &Package| p.entries.len())(pkg)); }'),
      'impl callback initializer': ('lib.rs', lambda s:s+'\nimpl Package { const READ: fn(&Package) -> usize = |pkg| pkg.entries.len(); }'),
      'enum discriminant body': ('lib.rs', lambda s:s+'\nenum Hidden { Value = (|pkg: &Package| pkg.entries.len())(PKG) }'),
      'type-level constant body': ('lib.rs', lambda s:s+'\ntype Hidden = [u8; (|pkg: &Package| pkg.entries.len())(PKG)];'),
      'macro-generated consumer': ('lib.rs', lambda s:s+'\nmacro_rules! hidden { ($f:ident) => { fn $f(pkg: &Package) -> usize { pkg.entries.len() } }; } hidden!(read);'),
      'macro-captured collection field': ('lib.rs', lambda s:s+'\nmacro_rules! hidden { ($f:ident) => { struct Hidden { $f: Vec<u8> } fn read(p: &Hidden) -> usize { p.$f.len() } }; } hidden!(brand_new);'),
      'contract macro starts emitting code': ('lib.rs', lambda s:s.replace('package_loss:literal;) => {};', 'package_loss:literal;) => { let _ = pkg.entries; };', 1)),
    })
    mutations.update({
      'contract text in literal is not a declaration': ('lib.rs', lambda s:s+'\nfn invented(pkg: &Package) { let text = r#"observation_contract! { rules: \"-\"; collections: \"field.entries*1\"; scope: \"x\"; loss: \"x\"; fallback: \"x\"; package_loss: \"independent\"; }"#; pkg.entries.len(); }'),
      'standard macro shadow hides consuming expansion': ('lib.rs', lambda s:s+'\nmacro_rules! format { ($field:ident) => { fn hidden(pkg: &Package) -> usize { pkg.$field.len() } }; } fn outer(pkg: &Package) -> usize { format!(entries); hidden(pkg) }'),
      'standard macro import shadow': ('lib.rs', lambda s:s+'\nuse external::format;'),
      'standard macro alias shadow': ('lib.rs', lambda s:s+'\nuse external::hidden as r#matches;'),
      'glob import could hide standard macro shadow': ('lib.rs', lambda s:s+'\nuse external::*;'),
      'attribute macro on item invocation': ('scan.rs', lambda s:s.replace('numeric_id!(DecimalId, 10);', '#[hidden] numeric_id!(DecimalId, 10);', 1)),
      'attribute macro on definition': ('scan.rs', lambda s:s.replace('macro_rules! numeric_id', '#[hidden] macro_rules! numeric_id', 1)),
      'external module without supplied source': ('lib.rs', lambda s:s+'\nmod hidden;'),
      'Unicode combining collection field': ('lib.rs', lambda s:s+'\nstruct UnicodeCollection { fresh\u0301: Vec<u8> } fn hidden(p: &UnicodeCollection) -> usize { p.fresh\u0301.len() }'),
      'Unicode combining membership receiver': ('lib.rs', lambda s:s+'\nfn hidden(fresh\u0301: &[u8]) -> bool { fresh\u0301.contains(&1) }'),
      'Unicode combining parser identifier': ('lib.rs', lambda s:s+'\nfn parse_fresh\u0301() -> usize { 1 } fn hidden() -> usize { parse_fresh\u0301() }'),
      'undeclared representation consumer': ('observed.rs', lambda s:s+'\nfn read(pkg: &Package) -> usize { pkg.entries.len() }'),
      'representation membership bypass': ('observed.rs', lambda s:s.replace('!self.0.contains(key)', '!self.0.contains(key) || raw.contains(key)', 1)),
    })
    # The new-accessor mutation must introduce a different access kind to test exact declaration matching.
    mutations['new undeclared accessor']=('lib.rs', lambda s:s.replace('acc.ran("CUSTOMXML-SCHEMA");','acc.ran("CUSTOMXML-SCHEMA"); let _ = new_collection.retain_budget(n, cap);',1))
    for label,(name,mutation) in mutations.items():
        changed=dict(sources);changed[name]=mutation(changed[name])
        if changed[name] == sources[name]: raise ValueError('mutation did not change source: '+label)
        try: declarations(changed)
        except ValueError as error: print('rejected:',label, '-', str(error).splitlines()[0])
        else: raise ValueError('audit accepted mutation: '+label)
    print(f'{len(mutations)} source mutations rejected')
    assert declarations(sources)==rows, "mutation audit contaminated the original sources"
    # Positive corpus: every current production construct is parsed and audited above.
    # Additional non-consuming bodies exercise recognition without declarations.
    for control in [
        'fn multiple() { consume(|a| { a }, |b| { b }); consume(|| { 1 }); }',
        'fn nested() { consume(|a| consume(|b| { a + b })); }',
        'fn r#raw_name(r#value: u8) -> u8 { r#value }',
        'fn fresh\u0301(value\u0301: u8) -> u8 { value\u0301 } fn script(℘: u8) -> u8 { ℘ }',
        'macro_rules! make_item { ($name:ident) => { fn $name(x: u8) -> u8 { x } }; } make_item!(fresh);',
        'const C: usize = { 1 }; enum Extra { A = 1 } type Array = [u8; 1];',
        'fn boolean() { if a || !matches!(b, A | B) { return; } consume(|x| { x }); }',
        'const TEXT: &str = r#"|Package { entries, .. }|"#; // |Package { entries, .. }|',
        '/* outer /* inner */ |Package { entries, .. }| */ fn explicit() {}',
        '#[cfg(test)] mod tests { fn read() { consume(|Package { entries, .. }| entries.len()); } }',
        'const F: fn(u8) -> u8 = |x| x; static S: fn(u8) -> u8 = |x| (|y| y)(x);',
    ]:
        changed = dict(sources); changed['lib.rs'] += '\n' + control
        assert declarations(changed) == rows
    print('accepted: current production corpus and non-consuming body controls')
    original=outputs(rows)
    try: verify_output('in-memory register', original[0]+'drift', original[0])
    except ValueError: print('rejected: generated register drift (exact byte comparison)')
    else: raise ValueError('audit accepted generated drift')


def verify_output(name, actual, expected):
    if actual != expected: raise ValueError(f'{name} drift; regenerate observation contracts')


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check',action='store_true')
    parser.add_argument('--self-test',action='store_true')
    args=parser.parse_args()
    root=Path(__file__).resolve().parents[1]
    sources={p.name:p.read_text() for p in (root/'src').glob('*.rs')}
    rows=declarations(sources)
    if args.self_test: self_test(sources,rows)
    values=outputs(rows)
    files=[root/'observation-contracts.md',root/'src/contracts_generated.rs']
    for path,value in zip(files,values):
        if args.check or args.self_test:
            verify_output(path.name, path.read_text() if path.exists() else '', value)
        else: path.write_text(value)
    print(f'{len(rows)} declared bounded consumers; register and access audit passed')

if __name__=='__main__':
    try: main()
    except (ValueError,AssertionError,OSError,subprocess.SubprocessError) as error:
        print(error,file=sys.stderr);sys.exit(1)
