#!/usr/bin/env python3
"""Generate and audit the validator's observation contracts using only Python's stdlib.

The audit checks declared collection consumption, typed-accessor bypasses and register drift.
It deliberately does not certify the semantic truth of a declaration; fixtures do that.
"""
from pathlib import Path
import argparse, json, re, sys
from collections import Counter

FIELDS = {
    'comment_defs','comment_refs','comment_para_ids','docpr','bookmarks','revisions',
    'footnote_refs','endnote_refs','footnote_defs','endnote_defs','header_refs','footer_refs','style_refs','num_ids',
    'listings','special_notes','content_parts','content_part_exemptions','ink_content_parts',
    'refs','entries','rels','content_types', 'note_listings',
}
BASE_FIELDS = frozenset(FIELDS)
ACCESSORS = {'prefix','is_complete','project','into_complete_set','missing','append','retain_budget','push_bounded','invalidate'}


def mask(text):
    """Blank Rust comments and literals, preserving all byte offsets and newlines."""
    pattern = r'//[^\n]*|/\*[\s\S]*?\*/|r(#+)?"[\s\S]*?"\1|"(?:\\.|[^"\\])*"|\'(?:\\.|[^\'\\])\''
    # Raw strings need a backreference with an empty group rather than an unmatched optional one.
    pattern = pattern.replace('(#+)?', '(#*)')
    return re.sub(pattern, lambda m: re.sub(r'[^\n]', ' ', m.group()), text)


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


def functions(text):
    code = mask(production(text))
    text = production(text)
    for m in re.finditer(r'\bfn\s+(\w+)\b', code):
        opening = -1
        parens = brackets = 0
        for at in range(m.end(), len(code)):
            char=code[at]
            if char == '(': parens += 1
            elif char == ')': parens -= 1
            elif char == '[': brackets += 1
            elif char == ']': brackets -= 1
            elif char == '{' and parens == brackets == 0:
                opening=at; break
            elif char == ';' and parens == brackets == 0:
                break
        if opening < 0: continue
        end = close_brace(code, opening)
        yield m.group(1), m.start(), opening, end, text[opening+1:end], code[opening+1:end]


def consumption(code):
    fields = ['field.'+m.group(1) for m in re.finditer(r'\.\s*(\w+)\b',code) if m.group(1) in FIELDS]
    methods = ['access.'+m.group(1) for m in re.finditer(r'\.\s*(\w+)\s*(?:\(|::)',code) if m.group(1) in ACCESSORS]
    parsers = ['parser.'+m.group(1) for m in re.finditer(r'\b(?:scan::)?(parse_\w+)\s*\(',code)]
    types = ['type.'+m.group() for m in re.finditer(r'\b(?:Observed|CompleteSet|Vec|HashSet|HashMap|BTreeSet|BTreeMap)\b', code)]
    return [f'{name}*{count}' for name,count in sorted(Counter(fields + methods + parsers + types).items())]


def discover_fields(sources):
    for name,text in sources.items():
        if name in {"model.rs", "reserved.rs", "contracts_generated.rs"} or name.endswith("_tests.rs"): continue
        code=mask(production(text))
        FIELDS.update(re.findall(r'^[ \t]*(?:pub(?:\(crate\))?[ \t]+)?(\w+)[ \t]*:[^\n]*(?:Observed|Vec|HashSet|HashMap|BTreeSet|BTreeMap)',code,re.M))


def audit_shapes(name, text):
    """Reject indirection shapes the lexical collection/access audit cannot follow.

    This is deliberately conservative, not Rust name resolution or dataflow analysis.
    Keep registry names at function scope so an alias or method pointer cannot escape
    the named consumer audit. Named-field patterns must not hide reads behind locals.
    """
    code = mask(production(text))
    for_patterns = list(re.finditer(r'\bfor\s+(?:(?!\bin\b)[^;={}])*\{', code))
    # Trait implementations also spell `for Type {`, but bind no field pattern.
    for_patterns = [m for m in for_patterns if not re.search(
        r'\bimpl\b', code[max(code.rfind(c, 0, m.start()) for c in ';{}')+1:m.start()])]
    match_patterns = any(re.match(r'[\]),\s]*(?:if[^{};]*)?=>', code[close_brace(code, m.end()-1)+1:])
                         for m in re.finditer(r'\b\w+\s*\{', code))
    if (re.search(r'\blet\b[^;={}]*\{', code)
        or for_patterns
        or match_patterns):
        raise ValueError(f'{name}: struct destructuring is unsupported in audited production code')
    module = list(code)
    for func, start, opening, end, body, masked in functions(text):
        if '{' in code[start:opening]:
            raise ValueError(f'{name}::{func}: destructured parameters are unsupported in audited production code')
        module[start:end+1] = ' ' * (end+1-start)
    module = ''.join(module)
    if name == 'ownership.rs':
        # Only the actual declaration and inherent impl may name the registry outside functions.
        module = re.sub(r'\b(?:struct|impl)\s+PackageRegistryContext\b', '', module)
    if re.search(r'\bPackageRegistryContext\b|(?:\.|::)\s*contexts\b', module):
        raise ValueError(f'{name}: module-scope package-registry aliases/queries are unsupported')


def rule_registry(sources):
    return set(re.findall(r'RuleSpec\s*\{\s*id:\s*"([A-Z0-9-]+)"', sources['lib.rs']))


def declarations(sources):
    rows=[];errors=[]
    FIELDS.clear(); FIELDS.update(BASE_FIELDS)
    discover_fields(sources)
    known_rules=rule_registry(sources)
    for name,text in sorted(sources.items()):
        if name.endswith('_tests.rs') or name in {'model.rs','reserved.rs','contracts_generated.rs'}:
            continue
        audit_shapes(name, text)
        for func,start,opening,end,body,code in functions(text):
            uses = consumption(mask(text[start:opening])+code)
            decl = re.search(r'observation_contract!\s*\{([\s\S]*?)\}', body)
            if name == 'observed.rs':
                # The implementation alone may inspect the representation. Callers cannot.
                continue
            if re.search(r'\.\s*(?:observed_value|observed_status)\b',code):
                errors.append(f'{name}::{func}: bypasses private observation representation')
            if 'PackageRegistryContext' in mask(text[start:opening])+code or re.search(r'(?:\.|::)\s*contexts\b',code):
                if (name,func) not in {('lib.rs','report_unreferenced_comments'),('lib.rs','check_custom_xml'),('ownership.rs','contexts')}:
                    errors.append(f'{name}::{func}: undeclared package-registry exception')
            if re.search(r'\.\s*difference\s*\(', code):
                errors.append(f'{name}::{func}: raw absence difference bypasses CompleteSet')
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
            for access in re.finditer(r'\b((?:\w+\s*\.\s*)*\w+)\s*\.\s*contains(?:_key)?\s*\(', code):
                receiver=re.sub(r'\s+', '', access.group(1))
                if (name,func,receiver) not in structural_membership:
                    errors.append(f'{name}::{func}: raw membership {receiver} bypasses CompleteSet or undeclared structural scope')
            if not uses and not decl and not re.search(r'\bacc\.(?:add|add_graded|ran)\(', code):
                continue
            if not decl:
                errors.append(f'{name}::{func}: bounded consumption has no observation contract: {uses}')
                continue
            data=dict(re.findall(r'(\w+)\s*:\s*"((?:\\.|[^"\\])*)"\s*;',decl.group(1)))
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
            rows.append(dict(helper=f'{name}::{func}', **data))
    covered={rule for row in rows for rule in row['rules'].split(',')}
    if known_rules-covered: errors.append(f'rule registry lacks observation contracts: {sorted(known_rules-covered)}')
    if errors:
        raise ValueError('\n'.join(errors))
    return rows


def outputs(rows):
    header = ('# Validator observation contracts\n\n'
        'Generated by `python3 crates/validate/observations/generate.py`; do not edit.\n'
        'Run with `--check` to audit consumers and drift; `--self-test` exercises rejection paths.\n\n'
        'Contracts record scope and loss policies for consumers recognized by the lexical audit; fixtures check their semantics.\nThe audit includes structural adapters and collection-typed locals, rejects named-field destructuring and module-scope registry aliases/queries, and checks exact declared footprints. It is not a general Rust alias or dataflow proof; Rust privacy seals the observation representation.\n'
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
      'module registry type alias': ('lib.rs',lambda s:s+'\ntype HiddenRegistry = ownership::PackageRegistryContext;\n'),
      'module registry method pointer': ('lib.rs',lambda s:s+"\nconst QUERY: for<'a> fn(&'a ownership::PackageRegistryContext, &str) -> &'a [OwnerId] = ownership::PackageRegistryContext::contexts;\n"),
    }
    # The new-accessor mutation must introduce a different access kind to test exact declaration matching.
    mutations['new undeclared accessor']=('lib.rs', lambda s:s.replace('acc.ran("CUSTOMXML-SCHEMA");','acc.ran("CUSTOMXML-SCHEMA"); let _ = new_collection.retain_budget(n, cap);',1))
    for label,(name,mutation) in mutations.items():
        changed=dict(sources);changed[name]=mutation(changed[name])
        try: declarations(changed)
        except ValueError: print('rejected:',label)
        else: raise ValueError('audit accepted mutation: '+label)
    assert declarations(sources)==rows, "mutation audit contaminated the original sources"
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
    except (ValueError,AssertionError) as error:
        print(error,file=sys.stderr);sys.exit(1)
