#!/usr/bin/env python3
"""Executed R18 fixtures: real copied generator --check, with optional isolated crate checks.
Run only on a stable tree; each mutation is asserted and restored in the disposable copy.
"""
from pathlib import Path
import json,os,re,shutil,subprocess,sys,tempfile,tomllib,hashlib
ROOT=Path(__file__).resolve().parents[1]
selected=set(sys.argv[1:])
RESULTS=[]
TMP=Path(tempfile.mkdtemp(prefix='wordcraft-r18-'))
COPY=TMP/'crates/validate'
shutil.copytree(ROOT/'crates/validate',COPY,ignore=shutil.ignore_patterns('__pycache__','target'))
(ROOT/'target/r18-probe-tool').mkdir(exist_ok=True)
(TMP/'target').symlink_to(ROOT/'target/r18-probe-tool',target_is_directory=True)
# A complete isolated validator crate, retaining all original sources/tests and locked versions.
manifest=(ROOT/'Cargo.toml').read_text()
lock=tomllib.loads((ROOT/'Cargo.lock').read_text())
versions={p['name']:p['version'] for p in lock['package'] if p['name'] in {'zip','quick-xml','serde','serde_json'}}
root_manifest='''[workspace]
resolver = "3"
members = ["crates/validate"]
[workspace.package]
version = "0.15.1"
edition = "2024"
license = "MIT OR Apache-2.0"
[workspace.dependencies]
'''
for n in versions:
 line=next(x for x in manifest.splitlines() if x.startswith(n+' = '))
 root_manifest+=line+'\n'
root_manifest+=manifest[manifest.index('[workspace.lints.rust]'):manifest.index('[profile.dev.package.')]
(TMP/'Cargo.toml').write_text(root_manifest)
shutil.copyfile(ROOT/'Cargo.lock',TMP/'Cargo.lock')
SRC=COPY/'src';GEN=COPY/'observations/generate.py'
originals={p.name:p.read_text() for p in SRC.glob('*.rs')}
original_outputs={p:p.read_bytes() for p in [COPY/'observation-contracts.md',SRC/'contracts_generated.rs']}
ctx={'__name__':'delta_fixtures'}
fixture=(ROOT/'log/delta17-probe.py').read_text().replace("ROOT = Path('/tmp/opencode/delta17-review')",f'ROOT = Path({str(TMP)!r})')
exec(compile(fixture,'log/delta17-probe.py','exec'),ctx)
# Map every delta-17 verbatim fixture to its binding red-team number.
labels={
1:'escape/remove-entry-truncation-absence-guard',2:'escape/raw-iterator-absence',3:'escape/helper-mediated-package-absence',4:'escape/existing-canonical-iterator-absence',
5:'escape/aliased-ufcs-observed-absence',6:'escape/self-ufcs-observed-absence',7:'escape/serde-content-types-consumer',8:'false-rejection/ordinary-named-field-binding',
9:'false-rejection/qualified-format',10:'false-rejection/inline-attribute',11:'false-rejection/trait-default-nonconsumer',12:'history/closure-parameter-destructure',13:'history/destructuring-assignment',14:'history/parenthesized-destructuring-assignment',15:'history/macro-produced-pattern',16:'history/raw-field',17:'history/raw-registry-method',18:'history/module-callback',19:'seam/type-alias-carrying-callback',20:'seam/trait-default-body',21:'seam/associated-const-callback',22:'seam/include-item',23:'seam/cfg-gated-consumer',24:'seam/other-macro-path',25:'seam/supported-macro-arguments-audited'}
def command(args,env=None):
 return subprocess.run(args,cwd=TMP,env={**os.environ,**(env or {})},capture_output=True,text=True,timeout=300)
def probe(number,label,name,mutation,accept=False,compile=False,test=None,diagnostic=None):
 if selected and str(number) not in selected:return
 path=SRC/name
 before=path.read_text();changed=mutation(before)
 assert changed!=before,(number,label,'fixture did not land')
 path.write_text(changed)
 record={'probe':number,'variant':label,'file':name,'fixture_sha256':hashlib.sha256(changed.encode()).hexdigest(),'expected':'accept' if accept else 'reject'}
 try:
  run=command(['python3',str(GEN),'--check']);record.update(exit=run.returncode,output=run.stdout+run.stderr)
  record['pass']=(run.returncode==0)==accept
  if diagnostic:
   record['diagnostic_expected']=diagnostic;record['pass'] &= diagnostic in record['output']
  if compile or test:
   args=['cargo','test','--offline','--lib',test] if test else ['cargo','check','--offline']
   check=command(args,{'CARGO_TARGET_DIR':str(ROOT/'target/r18-probe-compile')})
   record.update(compile_or_test_command=args,compile_or_test_exit=check.returncode,compile_or_test_output=check.stdout+check.stderr)
   record['pass'] &= check.returncode==0
  assert all(p.read_bytes()==b for p,b in original_outputs.items()),'probe altered register'
 finally:path.write_text(before)
 RESULTS.append(record)
 print(number,label,'PASS' if record['pass'] else 'FAIL',record['exit'],record['output'].splitlines()[0] if record['output'] else '',flush=True)
 (ROOT/'log/fredfix-round18-probe-results.json').write_text(json.dumps(RESULTS,indent=2)+'\n')
def condition(s,new):
 result,n=re.subn(r'if match package::canonical_entry\(pkg, target\) \{.*?\n            \} \{',new,s,count=1,flags=re.S)
 assert n==1
 return result
try:
 for n,label in labels.items():
  _,name,addition=next(p for p in ctx['PROBES'] if p[0]==label)
  if n==1:mutation=lambda s:condition(s,'if package::canonical_entry(pkg, target).is_none() {')
  else:mutation=lambda s,l=label,f=name,a=addition:ctx['mutate'](l,f,a)
  probe(n,label,name,mutation,accept=n in {4,9,10},compile=n in {4,8,9,10},test='round5_archive_prefix_absence' if n==4 else None)
 for pat,label in [('Resolution::Absent | Resolution::Unknown(_) => true,','or-pattern'),('_ => true,','wildcard')]:
  body='''if match package::canonical_entry(pkg, target) {
                Resolution::One(_) => false,
                PAT
                Resolution::Ambiguous(_) => false,
            } {'''.replace('PAT',pat)
  probe(26,label,'lib.rs',lambda s,b=body:condition(s,b),diagnostic='absence polarity')
 body='''if match package::canonical_entry(pkg, target) {
                Resolution::One(_) => false,
                Resolution::Absent => true,
                Resolution::Ambiguous(_) => false,
            } {'''
 probe(27,'missing Unknown','lib.rs',lambda s:condition(s,body),diagnostic='must explicitly cover')
 probe(28,'fabricated Absent','package.rs',lambda s:s.replace('Resolution::discovered(canonical(&pkg.entries, name), !pkg.entries_truncated && !pkg.identity_incomplete)','Resolution::Absent',1),diagnostic='literal Resolution::Absent')
 probe(28,'literal true completeness','package.rs',lambda s:s.replace('Resolution::discovered(canonical(&pkg.entries, name), !pkg.entries_truncated && !pkg.identity_incomplete)','Resolution::discovered(canonical(&pkg.entries, name), true)',1),diagnostic='completeness')
 contract='observation_contract! { rules: "-"; collections: "field.entries*1"; scope: "x"; loss: "x"; fallback: "x"; package_loss: "independent"; }'
 probe(29,'contracted raw absence','package.rs',lambda s:s+'\nfn unregistered_absence(entries: &[Entry], name: &str) -> bool { '+contract+' !entries.iter().any(|entry| entry.name == name) }',diagnostic='an observation contract cannot establish')
 for ty,param,op in [('type HiddenEntries = [Entry];','entries: &HiddenEntries','!entries.iter().any(|entry| entry.name == name)'),('type HiddenMap = HashMap<String, String>;','entries: &HiddenMap','entries.get(name).is_none()'),('type HiddenPackage = Package;','entries: &HiddenPackage','!entries.entries.iter().any(|entry| entry.name == name)')]:
  probe(30,ty,'package.rs',lambda s,t=ty,p=param,o=op:s+'\n'+t+' fn unregistered_absence('+p+', name: &str) -> bool { '+o+' }',diagnostic='raw absence')
 for helper in ['canonical_entry','entry_by_name','content_unavailable']:
  for binding in ['hidden_lookup','package::'+helper,'hidden_package::'+helper]:
   direct=binding=='hidden_lookup'
   addition=f'\nuse package::{helper} as hidden_lookup; use package as hidden_package; fn unregistered_absence(pkg: &Package, name: &str) -> bool {{ '+('let _ = hidden_lookup(pkg, name);' if direct else f'let lookup = {binding}; let _ = lookup(pkg, name);')+' false }'
   probe(31,helper+'/'+binding,'lib.rs',lambda s,a=addition:s+a,diagnostic='lookup.'+helper)
 for old,new,label in [
  ('pkg.entries_truncated || pkg.identity_incomplete','pkg.identity_incomplete','2751 delete'),
  ('pkg.entries_truncated || pkg.identity_incomplete','!pkg.entries_truncated || pkg.identity_incomplete','2751 negate'),
  ('!pkg.entries_truncated\n        && !pkg.identity_incomplete','!pkg.identity_incomplete','2777 delete'),
  ('!pkg.entries_truncated\n        && !pkg.identity_incomplete','pkg.entries_truncated\n        && !pkg.identity_incomplete','2777 negate')]:
  probe(32,label,'lib.rs',lambda s,o=old,n=new:s.replace(o,n,1),diagnostic='entry-loss')
 for callable in ['Self::prefix','HiddenDefinitions::prefix']:
  a='\ntype HiddenDefinitions = Observed<HashSet<String>>; impl Observed<HashSet<String>> { fn unregistered_absence(&self, key: &String) -> bool { let prefix = '+callable+'; !prefix(self).contains(key) } }'
  probe(33,callable,'lib.rs',lambda s,a=a:s+a,diagnostic='CompleteSet')
 old='self.parts.values().flat_map(|p| p.special_notes.prefix().iter().cloned()).collect()'
 for negative in [True,False]:
  body='self.parts.values().flat_map(|p| { let retained = p.special_notes.prefix(); retained.iter().filter(|candidate| '+('!' if negative else '')+'retained.contains(candidate)).cloned() }).collect()'
  probe(34,'exact prefix-only contract, negative='+str(negative),'lib.rs',lambda s,b=body:s.replace(old,b,1),accept=not negative,compile=not negative,diagnostic='CompleteSet' if negative else None)
 for expression in ['Self::prefix(self).contains(key) == false','match Self::prefix(self).contains(key) { false => true, true => false }','matches!(Self::prefix(self).contains(key), false)']:
  a='\nimpl Observed<HashSet<String>> { fn unregistered_absence(&self, key: &String) -> bool { '+expression+' } }'
  probe(35,expression,'lib.rs',lambda s,a=a:s+a,diagnostic='CompleteSet')
 for ty,expression in [('HashSet<String>','!Self::prefix(self).iter().any(|candidate| candidate == key)'),('HashMap<String, String>','Self::prefix(self).get(key).is_none()')]:
  a='\nimpl Observed<'+ty+'> { fn unregistered_absence(&self, key: &String) -> bool { '+expression+' } }'
  probe(36,expression,'lib.rs',lambda s,a=a:s+a,diagnostic='CompleteSet')
 for prefix in ['<Self>::prefix(self)','<Observed<HashSet<String>>>::prefix(self)']:
  a='\nimpl Observed<HashSet<String>> { fn unregistered_absence(&self, key: &String) -> bool { !'+prefix+'.contains(key) } }'
  probe(37,'negative/'+prefix,'lib.rs',lambda s,a=a:s+a,diagnostic='CompleteSet')
 for expression in ['Observed::is_complete(&self)','Self::is_complete(&self)','HiddenObservation::is_complete(&self)','<Self>::is_complete(&self)','<Observed<HashSet<K>>>::is_complete(&self)','{ let test = Self::is_complete; test(&self) }']:
  probe(37,'CompleteSet provider/'+expression,'observed.rs',lambda s,e=expression:s.replace('if self.is_complete() {','if '+e+' {',1)+'\ntype HiddenObservation<T> = Observed<T>;\n',accept=True,test='observed::tests')
 # Later stage probes are installed below this marker.
finally:
 shutil.rmtree(TMP)
failed=[(r['probe'],r['variant']) for r in RESULTS if not r['pass']]
print('failures:',failed,flush=True)
raise SystemExit(bool(failed))
