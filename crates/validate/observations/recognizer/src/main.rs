//! Closed syntax recognition for the observation gate. Unknown syntax is an error.
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use proc_macro2::{Delimiter, Group, TokenStream, TokenTree};
use quote::{ToTokens, quote};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::{self, BufRead, Write},
};
use syn::{
    parse::Parser,
    punctuated::Punctuated,
    visit::{self, Visit},
    *,
};

type Check<T> = std::result::Result<T, String>;

fn tokens(node: &impl ToTokens) -> String {
    fn normalize(stream: TokenStream) -> TokenStream {
        stream
            .into_iter()
            .map(|t| match t {
                TokenTree::Ident(i) => TokenTree::Ident(syn::ext::IdentExt::unraw(&i)),
                TokenTree::Group(g) => TokenTree::Group(Group::new(g.delimiter(), normalize(g.stream()))),
                t => t,
            })
            .collect()
    }
    normalize(node.to_token_stream()).to_string()
}
fn ident(node: &proc_macro2::Ident) -> String {
    node.to_string().trim_start_matches("r#").to_owned()
}
fn bad(kind: &str, node: &impl ToTokens) -> String {
    format!("unrecognized {kind}: {}", tokens(node).chars().take(180).collect::<String>())
}

#[derive(Clone)]
struct Template {
    matcher: Vec<TokenTree>,
    body: TokenStream,
}

// No repetition, alternative arms, nested matchers, or non-ident/literal fragments.
// Every accepted template is instantiated and parsed, including unused definitions.
fn template(mac: &Macro) -> Check<Template> {
    let ts: Vec<_> = mac.tokens.clone().into_iter().collect();
    match ts.as_slice() {
        [TokenTree::Group(matcher), TokenTree::Punct(eq), TokenTree::Punct(gt), TokenTree::Group(body), rest @ ..]
            if matcher.delimiter() == Delimiter::Parenthesis
                && eq.as_char() == '='
                && gt.as_char() == '>'
                && body.delimiter() == Delimiter::Brace
                && (rest.is_empty() || matches!(rest, [TokenTree::Punct(p)] if p.as_char() == ';')) =>
        {
            let result = Template { matcher: matcher.stream().into_iter().collect(), body: body.stream() };
            let _ = instantiate(&result, None)?;
            Ok(result)
        }
        _ => Err(bad("macro definition", mac)),
    }
}

fn instantiate(template: &Template, args: Option<TokenStream>) -> Check<TokenStream> {
    let mut input = args.map(|a| a.into_iter());
    let mut captures = HashMap::new();
    let mut matcher = template.matcher.iter();
    while let Some(token) = matcher.next() {
        if matches!(token, TokenTree::Punct(p) if p.as_char() == '$') {
            let (Some(TokenTree::Ident(name)), Some(TokenTree::Punct(colon)), Some(TokenTree::Ident(kind))) =
                (matcher.next(), matcher.next(), matcher.next())
            else {
                return Err("unsupported macro matcher".into());
            };
            if colon.as_char() != ':' {
                return Err("unsupported macro fragment".into());
            }
            let dummy: TokenStream = match ident(kind).as_str() {
                "ident" => "AuditPlaceholder".parse(),
                "literal" => "10".parse(),
                _ => return Err("unsupported macro fragment kind".into()),
            }
            .map_err(|e| format!("macro placeholder: {e}"))?;
            let value = if let Some(ref mut args) = input {
                args.next().ok_or("missing macro argument")?
            } else {
                dummy.into_iter().next().ok_or("missing placeholder")?
            };
            let valid = match ident(kind).as_str() {
                "ident" => matches!(value, TokenTree::Ident(_)),
                "literal" => matches!(value, TokenTree::Literal(_)),
                _ => false,
            };
            if !valid || captures.insert(ident(name), value).is_some() {
                return Err("unsupported macro argument".into());
            }
        } else {
            if !matches!(token, TokenTree::Ident(_) | TokenTree::Punct(_)) {
                return Err("unsupported macro matcher token".into());
            }
            if let Some(ref mut args) = input
                && args.next().as_ref().map(tokens) != Some(tokens(token))
            {
                return Err("macro argument mismatch".into());
            }
        }
    }
    if input.is_some_and(|mut a| a.next().is_some()) {
        return Err("extra macro arguments".into());
    }
    substitute(template.body.clone(), &captures)
}

fn substitute(stream: TokenStream, captures: &HashMap<String, TokenTree>) -> Check<TokenStream> {
    let mut out = TokenStream::new();
    let mut input = stream.into_iter();
    while let Some(token) = input.next() {
        match token {
            TokenTree::Punct(p) if p.as_char() == '$' => {
                let Some(TokenTree::Ident(name)) = input.next() else {
                    return Err("unsupported macro substitution".into());
                };
                out.extend([captures.get(&ident(&name)).ok_or("unknown macro capture")?.clone()]);
            }
            TokenTree::Group(g) => out.extend([TokenTree::Group(Group::new(g.delimiter(), substitute(g.stream(), captures)?))]),
            token => out.extend([token]),
        }
    }
    Ok(out)
}

struct Audit {
    error: Option<String>,
    units: Vec<Value>,
    surface: Vec<String>,
    enum_variants: Vec<String>,
    collection_fields: Vec<String>,
    modules: Vec<String>,
    templates: HashMap<String, Template>,
    depth: usize,
    in_unit: usize,
    syntax_depth: usize,
}
impl Audit {
    fn fail(&mut self, message: String) {
        if self.error.is_none() {
            self.error = Some(message);
        }
    }
    fn unit(&mut self, name: String, header: String, body: String) {
        self.units.push(json!({"name":name,"header":header,"body":body}));
    }
    fn function(&mut self, sig: &Signature, block: &Block) {
        self.unit(ident(&sig.ident), tokens(sig), tokens(block));
        self.in_unit += 1;
        self.visit_signature(sig);
        self.visit_block(block);
        self.in_unit -= 1;
    }
    fn initializer(&mut self, name: &proc_macro2::Ident, ty: &Type, expr: &Expr) {
        self.unit(format!("<initializer {}>", ident(name)), tokens(ty), tokens(expr));
        self.in_unit += 1;
        self.visit_type(ty);
        self.visit_expr(expr);
        self.in_unit -= 1;
    }
    fn expanded_items(&mut self, stream: TokenStream) {
        if self.depth >= 32 {
            self.fail("macro expansion depth exceeded".into());
            return;
        }
        match syn::parse2::<File>(stream) {
            Ok(file) => {
                self.depth += 1;
                self.visit_file(&file);
                self.depth -= 1;
            }
            Err(e) => self.fail(format!("unsupported item macro expansion: {e}")),
        }
    }
    fn expression_list(&mut self, stream: TokenStream) {
        match Punctuated::<Expr, Token![,]>::parse_terminated.parse2(stream) {
            Ok(exprs) => {
                for expr in exprs {
                    self.visit_expr(&expr);
                }
            }
            Err(e) => self.fail(format!("unsupported macro expression arguments: {e}")),
        }
    }
}
impl<'ast> Visit<'ast> for Audit {
    fn visit_type_param_bound(&mut self, bound: &'ast TypeParamBound) {
        match bound {
            TypeParamBound::Trait(_) | TypeParamBound::Lifetime(_) => visit::visit_type_param_bound(self, bound),
            _ => self.fail(bad("type parameter bound", bound)),
        }
    }
    fn visit_lit(&mut self, lit: &'ast Lit) {
        match lit {
            Lit::Str(_) | Lit::ByteStr(_) | Lit::CStr(_) | Lit::Byte(_) | Lit::Char(_) | Lit::Int(_) | Lit::Float(_) | Lit::Bool(_) => {
                visit::visit_lit(self, lit)
            }
            _ => self.fail(bad("literal", lit)),
        }
    }
    fn visit_token_stream(&mut self, stream: &'ast TokenStream) {
        self.fail(bad("unparsed token stream", stream));
    }
    fn visit_use_tree(&mut self, tree: &'ast UseTree) {
        // Imports must not replace a standard macro whose argument grammar we audit.
        // Glob imports conceal binding names and therefore cannot be recognized here.
        let binding = match tree {
            UseTree::Name(n) => Some(ident(&n.ident)),
            UseTree::Rename(r) => Some(ident(&r.rename)),
            UseTree::Glob(_) => {
                self.fail(bad("glob import", tree));
                return;
            }
            UseTree::Path(_) | UseTree::Group(_) => None,
        };
        if binding.is_some_and(|n| matches!(n.as_str(), "format" | "vec" | "matches" | "debug_assert" | "observation_contract")) {
            self.fail(bad("macro-shadowing import", tree));
            return;
        }
        visit::visit_use_tree(self, tree);
    }
    fn visit_field(&mut self, field: &'ast Field) {
        let ty = tokens(&field.ty);
        if ty
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .any(|n| matches!(n, "Observed" | "Vec" | "HashSet" | "HashMap" | "BTreeSet" | "BTreeMap"))
            && let Some(name) = &field.ident
        {
            self.collection_fields.push(ident(name));
        }
        visit::visit_field(self, field);
    }
    fn visit_attribute(&mut self, attr: &'ast Attribute) {
        // Attribute macros/cfg_attr may replace bodies and are outside this grammar.
        let name = tokens(attr.path());
        if !matches!(name.as_str(), "doc" | "derive" | "default" | "allow" | "deny" | "forbid" | "cfg") {
            self.fail(bad("attribute", attr));
        }
        // Only built-in and the already used serde derive are in scope.
        if name == "derive" {
            let Meta::List(list) = &attr.meta else {
                self.fail(bad("derive", attr));
                return;
            };
            match Punctuated::<Path, Token![,]>::parse_terminated.parse2(list.tokens.clone()) {
                Ok(paths)
                    if paths.iter().all(|p| {
                        matches!(
                            tokens(p).as_str(),
                            "Clone" | "Copy" | "Debug" | "Default" | "PartialEq" | "Eq" | "PartialOrd" | "Ord" | "Hash" | "Serialize"
                        )
                    }) => {}
                _ => self.fail(bad("derive", attr)),
            }
        }
    }
    fn visit_item(&mut self, item: &'ast Item) {
        if let Item::Macro(m) = item {
            for attr in &m.attrs {
                self.visit_attribute(attr);
            }
        }
        match item {
            Item::Fn(f) => {
                for a in &f.attrs {
                    self.visit_attribute(a);
                }
                self.function(&f.sig, &f.block);
            }
            Item::Const(c) => {
                for a in &c.attrs {
                    self.visit_attribute(a);
                }
                self.visit_generics(&c.generics);
                self.initializer(&c.ident, &c.ty, &c.expr);
            }
            Item::Static(s) => {
                for a in &s.attrs {
                    self.visit_attribute(a);
                }
                self.initializer(&s.ident, &s.ty, &s.expr);
            }
            Item::Impl(i) => {
                let self_type = tokens(&i.self_ty);
                if self_type != "PackageRegistryContext" || i.trait_.is_some() {
                    self.surface.push(self_type);
                }
                visit::visit_item_impl(self, i);
            }
            Item::Struct(s) => {
                self.surface.push(tokens(&s.fields));
                visit::visit_item_struct(self, s);
            }
            Item::Enum(e) => {
                for v in &e.variants {
                    self.enum_variants.push(format!("{}::{}", ident(&e.ident), ident(&v.ident)));
                }
                self.surface.push(tokens(&e.variants));
                visit::visit_item_enum(self, e);
            }
            Item::Type(t) => {
                self.surface.push(tokens(t));
                visit::visit_item_type(self, t);
            }
            Item::Use(u) => {
                self.surface.push(tokens(u));
                visit::visit_item_use(self, u);
            }
            Item::Mod(m)
                if m.content.is_some()
                    && m.attrs.iter().any(|a| matches!(&a.meta, Meta::List(l) if l.path.is_ident("cfg") && l.tokens.to_string() == "test")) => {} // Inline unit tests are explicitly outside the production audit.
            Item::Mod(m) if m.content.is_none() => {
                self.modules.push(ident(&m.ident));
                visit::visit_item_mod(self, m);
            }
            Item::Macro(m) if tokens(&m.mac.path) == "macro_rules" => {
                let Some(name) = &m.ident else {
                    self.fail(bad("macro definition", m));
                    return;
                };
                if matches!(ident(name).as_str(), "format" | "vec" | "matches" | "debug_assert") {
                    self.fail("standard expression macro shadowing is outside the closed grammar".into());
                    return;
                }
                match template(&m.mac) {
                    Ok(t) => {
                        match instantiate(&t, None) {
                            Ok(stream) => self.expanded_items(stream),
                            Err(e) => self.fail(e),
                        }
                        if self.templates.insert(ident(name), t).is_some() {
                            self.fail("macro shadowing is unsupported".into());
                        }
                    }
                    Err(e) => self.fail(e),
                }
            }
            Item::Macro(m) => {
                let name = tokens(&m.mac.path);
                if let Some(t) = self.templates.get(&name).cloned() {
                    match instantiate(&t, Some(m.mac.tokens.clone())) {
                        Ok(stream) => self.expanded_items(stream),
                        Err(e) => self.fail(e),
                    }
                } else {
                    self.fail(bad("item macro", m));
                }
            }
            // Traits, unions, inline production modules, foreign items, and verbatim items
            // have no recognized production in this grammar. Never skip their bodies.
            _ => self.fail(bad("item", item)),
        }
    }
    fn visit_impl_item(&mut self, item: &'ast ImplItem) {
        match item {
            ImplItem::Fn(f) => {
                for a in &f.attrs {
                    self.visit_attribute(a);
                }
                self.function(&f.sig, &f.block);
            }
            ImplItem::Const(c) => {
                for a in &c.attrs {
                    self.visit_attribute(a);
                }
                self.visit_generics(&c.generics);
                self.initializer(&c.ident, &c.ty, &c.expr);
            }
            ImplItem::Type(t) => {
                self.surface.push(tokens(t));
                visit::visit_impl_item_type(self, t);
            }
            _ => self.fail(bad("impl item", item)),
        }
    }
    fn visit_pat(&mut self, pat: &'ast Pat) {
        match pat {
            Pat::Ident(_)
            | Pat::Lit(_)
            | Pat::Or(_)
            | Pat::Paren(_)
            | Pat::Path(_)
            | Pat::Range(_)
            | Pat::Reference(_)
            | Pat::Rest(_)
            | Pat::Slice(_)
            | Pat::Tuple(_)
            | Pat::TupleStruct(_)
            | Pat::Type(_)
            | Pat::Wild(_) => visit::visit_pat(self, pat),
            _ => self.fail(bad("pattern", pat)),
        }
    }
    fn visit_type(&mut self, ty: &'ast Type) {
        match ty {
            Type::Array(_)
            | Type::BareFn(_)
            | Type::Group(_)
            | Type::ImplTrait(_)
            | Type::Infer(_)
            | Type::Never(_)
            | Type::Paren(_)
            | Type::Path(_)
            | Type::Reference(_)
            | Type::Slice(_)
            | Type::TraitObject(_)
            | Type::Tuple(_) => visit::visit_type(self, ty),
            _ => self.fail(bad("type", ty)),
        }
    }
    fn visit_expr(&mut self, expr: &'ast Expr) {
        // Expressions in discriminants, type-level constants and other structural
        // positions are audited too, even when there is no enclosing named body.
        if self.error.is_some() {
            return;
        }
        if self.syntax_depth >= 256 {
            self.fail("syntax depth exceeded".into());
            return;
        }
        if self.in_unit == 0 {
            self.unit("<structural expression>".into(), String::new(), tokens(expr));
        }
        self.in_unit += 1;
        self.syntax_depth += 1;
        match expr {
            Expr::Assign(a) => {
                fn place(e: &Expr) -> bool {
                    match e {
                        Expr::Path(_) | Expr::Field(_) | Expr::Index(_) => true,
                        Expr::Unary(u) => matches!(u.op, UnOp::Deref(_)),
                        Expr::Paren(p) => place(&p.expr),
                        Expr::Group(g) => place(&g.expr),
                        _ => false,
                    }
                }
                if !place(&a.left) {
                    self.fail(bad("assignment target", &a.left));
                }
                visit::visit_expr(self, expr);
            }
            Expr::Array(_)
            | Expr::Binary(_)
            | Expr::Block(_)
            | Expr::Break(_)
            | Expr::Call(_)
            | Expr::Cast(_)
            | Expr::Closure(_)
            | Expr::Continue(_)
            | Expr::Field(_)
            | Expr::ForLoop(_)
            | Expr::Group(_)
            | Expr::If(_)
            | Expr::Index(_)
            | Expr::Let(_)
            | Expr::Lit(_)
            | Expr::Loop(_)
            | Expr::Macro(_)
            | Expr::Match(_)
            | Expr::MethodCall(_)
            | Expr::Paren(_)
            | Expr::Path(_)
            | Expr::Range(_)
            | Expr::Reference(_)
            | Expr::Repeat(_)
            | Expr::Return(_)
            | Expr::Struct(_)
            | Expr::Try(_)
            | Expr::Tuple(_)
            | Expr::Unary(_)
            | Expr::While(_) => visit::visit_expr(self, expr),
            _ => self.fail(bad("expression", expr)),
        }
        self.syntax_depth -= 1;
        self.in_unit -= 1;
    }
    fn visit_macro(&mut self, mac: &'ast Macro) {
        let name = tokens(&mac.path);
        match name.as_str() {
            "observation_contract" => {
                // Validate the actual no-code template, not just the invocation name.
                match self
                    .templates
                    .get(&name)
                    .ok_or("undeclared observation contract macro".into())
                    .and_then(|t| instantiate(t, Some(mac.tokens.clone())))
                {
                    Ok(stream) if stream.is_empty() => {}
                    Ok(_) => self.fail("observation contract macro emits code".into()),
                    Err(e) => self.fail(e),
                }
            }
            "format" | "debug_assert" => self.expression_list(mac.tokens.clone()),
            "vec" => {
                // Vec's two public forms contain expressions only.
                let ts = mac.tokens.clone();
                if let Ok(repeat) = syn::parse2::<ExprRepeat>(quote!([#ts])) {
                    self.visit_expr(&repeat.expr);
                    self.visit_expr(&repeat.len);
                } else {
                    self.expression_list(ts);
                }
            }
            "matches" => {
                struct Matches {
                    expr: Expr,
                    pat: Pat,
                    guard: Option<Expr>,
                }
                impl syn::parse::Parse for Matches {
                    fn parse(input: syn::parse::ParseStream<'_>) -> syn::Result<Self> {
                        let expr = input.parse()?;
                        input.parse::<Token![,]>()?;
                        let pat = Pat::parse_multi_with_leading_vert(input)?;
                        let guard = if input.peek(Token![if]) {
                            input.parse::<Token![if]>()?;
                            Some(input.parse()?)
                        } else {
                            None
                        };
                        if input.peek(Token![,]) {
                            input.parse::<Token![,]>()?;
                        }
                        Ok(Self { expr, pat, guard })
                    }
                }
                match syn::parse2::<Matches>(mac.tokens.clone()) {
                    Ok(m) => {
                        self.visit_expr(&m.expr);
                        self.visit_pat(&m.pat);
                        if let Some(g) = m.guard {
                            self.visit_expr(&g);
                        }
                    }
                    Err(e) => self.fail(format!("unsupported matches arguments: {e}")),
                }
            }
            _ => self.fail(bad("expression/pattern macro", mac)),
        }
    }
}

fn audit(source: &str, templates: HashMap<String, Template>) -> Check<(Value, HashMap<String, Template>)> {
    let file = syn::parse_file(source).map_err(|e| format!("Rust parse failed: {e}"))?;
    let mut audit = Audit {
        error: None,
        units: vec![],
        surface: vec![],
        enum_variants: vec![],
        collection_fields: vec![],
        modules: vec![],
        templates,
        depth: 0,
        in_unit: 0,
        syntax_depth: 0,
    };
    audit.visit_file(&file);
    if let Some(e) = audit.error {
        return Err(e);
    }
    Ok((
        json!({"units":audit.units,"surface":audit.surface,"enum_variants":audit.enum_variants,"collection_fields":audit.collection_fields,"modules":audit.modules}),
        audit.templates,
    ))
}
fn request(value: Value) -> Check<Value> {
    let sources = value.as_object().ok_or("expected source map")?;
    let mut result = serde_json::Map::new();
    let mut templates = HashMap::new();
    // lib.rs defines the no-code declaration macro for its child modules.
    for name in ["lib.rs"].into_iter().chain(sources.keys().map(String::as_str).filter(|n| *n != "lib.rs")) {
        if let Some(source) = sources.get(name) {
            let (data, next) = audit(source.as_str().ok_or("expected source text")?, templates.clone()).map_err(|e| format!("{name}: {e}"))?;
            result.insert(name.into(), data);
            if name == "lib.rs" {
                templates = next;
            }
        }
    }
    Ok(Value::Object(result))
}
fn run() -> Check<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let response = match line.map_err(|e| e.to_string()).and_then(|line| serde_json::from_str(&line).map_err(|e| e.to_string())).and_then(request)
        {
            Ok(value) => json!({"ok":value}),
            Err(e) => json!({"error":e}),
        };
        writeln!(stdout, "{response}").map_err(|e| e.to_string())?;
        stdout.flush().map_err(|e| e.to_string())?;
    }
    Ok(())
}
fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
