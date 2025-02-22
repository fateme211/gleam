#[cfg(test)]
mod tests;

use crate::{
    ast::{
        CustomType, Function, Import, ModuleConstant, TypeAlias, TypeAstConstructor, TypeAstFn,
        TypeAstHole, TypeAstTuple, TypeAstVar, Use, *,
    },
    build::Target,
    docvec,
    io::Utf8Writer,
    parse::extra::{Comment, ModuleExtra},
    pretty::{self, *},
    type_::{self, Type},
    Error, Result,
};
use ecow::EcoString;
use itertools::Itertools;
use std::sync::Arc;
use vec1::Vec1;

use crate::type_::Deprecation;
use camino::Utf8Path;

const INDENT: isize = 2;

pub fn pretty(writer: &mut impl Utf8Writer, src: &EcoString, path: &Utf8Path) -> Result<()> {
    let parsed = crate::parse::parse_module(src).map_err(|error| Error::Parse {
        path: path.to_path_buf(),
        src: src.clone(),
        error,
    })?;
    let intermediate = Intermediate::from_extra(&parsed.extra, src);
    Formatter::with_comments(&intermediate)
        .module(&parsed.module)
        .pretty_print(80, writer)
}

pub(crate) struct Intermediate<'a> {
    comments: Vec<Comment<'a>>,
    doc_comments: Vec<Comment<'a>>,
    module_comments: Vec<Comment<'a>>,
    empty_lines: &'a [u32],
}

impl<'a> Intermediate<'a> {
    pub fn from_extra(extra: &'a ModuleExtra, src: &'a EcoString) -> Intermediate<'a> {
        Intermediate {
            comments: extra
                .comments
                .iter()
                .map(|span| Comment::from((span, src)))
                .collect(),
            doc_comments: extra
                .doc_comments
                .iter()
                .map(|span| Comment::from((span, src)))
                .collect(),
            empty_lines: &extra.empty_lines,
            module_comments: extra
                .module_comments
                .iter()
                .map(|span| Comment::from((span, src)))
                .collect(),
        }
    }
}

/// Hayleigh's bane
#[derive(Debug, Clone, Default)]
pub struct Formatter<'a> {
    comments: &'a [Comment<'a>],
    doc_comments: &'a [Comment<'a>],
    module_comments: &'a [Comment<'a>],
    empty_lines: &'a [u32],
}

impl<'comments> Formatter<'comments> {
    pub fn new() -> Self {
        Default::default()
    }

    pub(crate) fn with_comments(extra: &'comments Intermediate<'comments>) -> Self {
        Self {
            comments: &extra.comments,
            doc_comments: &extra.doc_comments,
            module_comments: &extra.module_comments,
            empty_lines: extra.empty_lines,
        }
    }

    fn any_comments(&self, limit: u32) -> bool {
        self.comments
            .first()
            .map(|comment| comment.start < limit)
            .unwrap_or(false)
    }

    fn any_empty_lines(&self, limit: u32) -> bool {
        self.empty_lines
            .first()
            .map(|line| *line < limit)
            .unwrap_or(false)
    }

    /// Pop comments that occur before a byte-index in the source, consuming
    /// and retaining any empty lines contained within.
    fn pop_comments(&mut self, limit: u32) -> impl Iterator<Item = Option<&'comments str>> {
        let (popped, rest, empty_lines) =
            comments_before(self.comments, self.empty_lines, limit, true);
        self.comments = rest;
        self.empty_lines = empty_lines;
        popped
    }

    /// Pop doc comments that occur before a byte-index in the source, consuming
    /// and dropping any empty lines contained within.
    fn pop_doc_comments(&mut self, limit: u32) -> impl Iterator<Item = Option<&'comments str>> {
        let (popped, rest, empty_lines) =
            comments_before(self.doc_comments, self.empty_lines, limit, false);
        self.doc_comments = rest;
        self.empty_lines = empty_lines;
        popped
    }

    /// Remove between 0 and `limit` empty lines following the current position,
    /// returning true if any empty lines were removed.
    fn pop_empty_lines(&mut self, limit: u32) -> bool {
        let mut end = 0;
        for (i, &position) in self.empty_lines.iter().enumerate() {
            if position > limit {
                break;
            }
            end = i + 1;
        }

        self.empty_lines = self
            .empty_lines
            .get(end..)
            .expect("Pop empty lines slicing");
        end != 0
    }

    fn targeted_definition<'a>(&mut self, definition: &'a TargetedDefinition) -> Document<'a> {
        let target = definition.target;
        let definition = &definition.definition;
        let start = definition.location().start;
        let comments = self.pop_comments(start);
        let document = self.documented_definition(definition);
        let document = match target {
            None => document,
            Some(Target::Erlang) => docvec!["@target(erlang)", line(), document],
            Some(Target::JavaScript) => docvec!["@target(javascript)", line(), document],
        };
        commented(document, comments)
    }

    pub(crate) fn module<'a>(&mut self, module: &'a UntypedModule) -> Document<'a> {
        let mut documents = vec![];
        let mut previous_was_a_definition = false;

        // Here we take consecutive groups of imports so that they can be sorted
        // alphabetically.
        for (is_import_group, definitions) in &module
            .definitions
            .iter()
            .group_by(|definition| definition.definition.is_import())
        {
            if is_import_group {
                if previous_was_a_definition {
                    documents.push(lines(2));
                }
                documents.append(&mut self.imports(definitions.collect_vec()));
                previous_was_a_definition = false;
            } else {
                for definition in definitions {
                    if !documents.is_empty() {
                        documents.push(lines(2));
                    }
                    documents.push(self.targeted_definition(definition));
                }
                previous_was_a_definition = true;
            }
        }

        let definitions = concat(documents);

        // Now that definitions has been collected, only freestanding comments (//)
        // and doc comments (///) remain. Freestanding comments aren't associated
        // with any statement, and are moved to the bottom of the module.
        let doc_comments = join(
            self.doc_comments.iter().map(|comment| {
                "///"
                    .to_doc()
                    .append(Document::String(comment.content.to_string()))
            }),
            line(),
        );

        let comments = match printed_comments(self.pop_comments(u32::MAX), false) {
            Some(comments) => comments,
            None => nil(),
        };

        let module_comments = if !self.module_comments.is_empty() {
            let comments = self.module_comments.iter().map(|s| {
                "////"
                    .to_doc()
                    .append(Document::String(s.content.to_string()))
            });
            join(comments, line()).append(line())
        } else {
            nil()
        };

        let non_empty = vec![module_comments, definitions, doc_comments, comments]
            .into_iter()
            .filter(|doc| !doc.is_empty());

        join(non_empty, line()).append(line())
    }

    /// Separates the imports in groups delimited by comments or empty lines and
    /// sorts each group alphabetically.
    ///
    fn imports<'a>(&mut self, imports: Vec<&'a TargetedDefinition>) -> Vec<Document<'a>> {
        // The formatter needs to play nicely with import groups defined by the
        // programmer. If one puts a comment before an import then that's a clue
        // for the formatter that it has run into a gorup of related imports.
        //
        // So we can't just sort `imports` and format each one, we have to be a
        // bit smarter and see if each import is preceded by a comment.
        // Once we find a comment we know we're done with the current import
        // group and a new one has started.
        //
        // ```gleam
        // // This is an import group.
        // import gleam/int
        // import gleam/string
        //
        // // This marks the beginning of a new import group that can't
        // // be mushed together with the previous one!
        // import wibble
        // import wobble
        // ```
        let mut documents = vec![];
        let mut current_group = vec![];
        let mut group_comments = None;

        for import in imports {
            let start = import.definition.location().start;
            // If the import is preceded by a comment then we want to put it
            // into a new group and we can print the current one.
            if !current_group.is_empty()
                && (self.any_comments(start) || self.any_empty_lines(start))
            {
                documents.append(&mut self.sorted_import_group(&mut current_group, group_comments));
                documents.push(lines(2));
                // We pop the comment introducing the group and save it for
                // later for when the group is over and we can actually print
                // it.
                // We have to immediately pop the comment as soon as we start
                // with a new group or it would still be present in
                // `self.comments` messing up `self.any_comments` for the next
                // imports in the group.
                group_comments = printed_comments(self.pop_comments(start), false);
            }
            let _ = self.pop_empty_lines(start);
            current_group.push(import);
        }

        // Let's not forget about the last import group!
        if !current_group.is_empty() {
            documents.append(&mut self.sorted_import_group(&mut current_group, group_comments));
        }

        documents
    }

    /// Prints the imports as a single sorted group of import statements
    /// draining the given vector.
    ///
    /// `group_comment` is the comment preceding the current group.
    /// It might be missing since a group could also be defined by simply having
    /// an empty line between imports.
    ///
    fn sorted_import_group<'a>(
        &mut self,
        imports: &mut Vec<&'a TargetedDefinition>,
        group_comment: Option<Document<'a>>,
    ) -> Vec<Document<'a>> {
        let mut documents = Vec::with_capacity(imports.len() * 2);

        // If the group is defined with a single comment we print it as the
        // first thing.
        if let Some(comment) = group_comment {
            documents.push(comment)
        };

        imports.sort_by(|one, other| match (&one.definition, &other.definition) {
            (Definition::Import(one), Definition::Import(other)) => one.module.cmp(&other.module),
            // It shouldn't really be possible for a non import to be here so
            // we just return a default value.
            _ => std::cmp::Ordering::Equal,
        });

        for import in imports.iter() {
            if !documents.is_empty() {
                documents.push(lines(1))
            }
            documents.push(self.targeted_definition(import));
        }
        imports.clear();
        documents
    }

    fn definition<'a>(&mut self, statement: &'a UntypedDefinition) -> Document<'a> {
        match statement {
            Definition::Function(function) => self.statement_fn(function),

            Definition::TypeAlias(TypeAlias {
                alias,
                parameters: args,
                type_ast: resolved_type,
                public,
                deprecation,
                ..
            }) => self.type_alias(*public, alias, args, resolved_type, deprecation),

            Definition::CustomType(ct) => self.custom_type(ct),

            Definition::Import(Import {
                module,
                as_name,
                unqualified_values,
                unqualified_types,
                ..
            }) => {
                let second = if unqualified_values.is_empty() && unqualified_types.is_empty() {
                    nil()
                } else {
                    let unqualified_types = unqualified_types
                        .iter()
                        .sorted_by(|a, b| a.name.cmp(&b.name))
                        .map(|e| docvec!["type ", e]);
                    let unqualified_values = unqualified_values
                        .iter()
                        .sorted_by(|a, b| a.name.cmp(&b.name))
                        .map(|e| e.to_doc());
                    let unqualified = join(
                        unqualified_types.chain(unqualified_values),
                        flex_break(",", ", "),
                    );
                    let unqualified = break_("", "")
                        .append(unqualified)
                        .nest(INDENT)
                        .append(break_(",", ""))
                        .group();
                    ".{".to_doc().append(unqualified).append("}")
                };
                let doc = docvec!["import ", module.as_str(), second];
                match as_name {
                    Some((AssignName::Variable(name) | AssignName::Discard(name), _)) => {
                        doc.append(" as ").append(name)
                    }
                    None => doc,
                }
            }

            Definition::ModuleConstant(ModuleConstant {
                public,
                name,
                annotation,
                value,
                ..
            }) => {
                let head = pub_(*public).append("const ").append(name.as_str());
                let head = match annotation {
                    None => head,
                    Some(t) => head.append(": ").append(self.type_ast(t)),
                };
                head.append(" = ").append(self.const_expr(value))
            }
        }
    }

    fn const_expr<'a, A, B>(&mut self, value: &'a Constant<A, B>) -> Document<'a> {
        match value {
            Constant::Int { value, .. } => self.int(value),

            Constant::Float { value, .. } => self.float(value),

            Constant::String { value, .. } => self.string(value),

            Constant::List { elements, .. } => self.const_list(elements),

            Constant::Tuple { elements, .. } => "#"
                .to_doc()
                .append(wrap_args(elements.iter().map(|e| self.const_expr(e))))
                .group(),

            Constant::BitArray { segments, .. } => bit_array(
                segments
                    .iter()
                    .map(|s| bit_array_segment(s, |e| self.const_expr(e))),
                segments.iter().all(|s| s.value.is_simple()),
            ),

            Constant::Record {
                name,
                args,
                module: None,
                ..
            } if args.is_empty() => name.to_doc(),

            Constant::Record {
                name,
                args,
                module: Some(m),
                ..
            } if args.is_empty() => m.to_doc().append(".").append(name.as_str()),

            Constant::Record {
                name,
                args,
                module: None,
                ..
            } => name
                .to_doc()
                .append(wrap_args(args.iter().map(|a| self.constant_call_arg(a))))
                .group(),

            Constant::Record {
                name,
                args,
                module: Some(m),
                ..
            } => m
                .to_doc()
                .append(".")
                .append(name.as_str())
                .append(wrap_args(args.iter().map(|a| self.constant_call_arg(a))))
                .group(),

            Constant::Var {
                name, module: None, ..
            } => name.to_doc(),

            Constant::Var {
                name,
                module: Some(module),
                ..
            } => docvec![module, ".", name],
        }
    }

    fn const_list<'a, A, B>(&mut self, elements: &'a [Constant<A, B>]) -> Document<'a> {
        if elements.is_empty() {
            return "[]".to_doc();
        }

        let comma: fn() -> Document<'a> = if elements.iter().all(Constant::is_simple) {
            || flex_break(",", ", ")
        } else {
            || break_(",", ", ")
        };
        docvec![
            break_("[", "["),
            join(elements.iter().map(|e| self.const_expr(e)), comma())
        ]
        .nest(INDENT)
        .append(break_(",", ""))
        .append("]")
        .group()
    }

    pub fn docs_const_expr<'a>(
        &mut self,
        public: bool,
        name: &'a str,
        value: &'a TypedConstant,
    ) -> Document<'a> {
        let mut printer = type_::pretty::Printer::new();

        pub_(public)
            .append("const ")
            .append(name)
            .append(": ")
            .append(printer.print(&value.type_()))
    }

    fn documented_definition<'a>(&mut self, s: &'a UntypedDefinition) -> Document<'a> {
        let comments = self.doc_comments(s.location().start);
        comments.append(self.definition(s).group()).group()
    }

    fn doc_comments<'a>(&mut self, limit: u32) -> Document<'a> {
        let mut comments = self.pop_doc_comments(limit).peekable();
        match comments.peek() {
            None => nil(),
            Some(_) => join(
                comments.map(|c| match c {
                    Some(c) => "///".to_doc().append(Document::String(c.to_string())),
                    None => unreachable!("empty lines dropped by pop_doc_comments"),
                }),
                line(),
            )
            .append(line())
            .force_break(),
        }
    }

    fn type_ast_constructor<'a>(
        &mut self,
        module: &'a Option<EcoString>,
        name: &'a str,
        args: &'a [TypeAst],
    ) -> Document<'a> {
        let head = module
            .as_ref()
            .map(|qualifier| qualifier.to_doc().append(".").append(name))
            .unwrap_or_else(|| name.to_doc());

        if args.is_empty() {
            head
        } else {
            head.append(self.type_arguments(args))
        }
    }

    fn type_ast<'a>(&mut self, t: &'a TypeAst) -> Document<'a> {
        match t {
            TypeAst::Hole(TypeAstHole { name, .. }) => name.to_doc(),

            TypeAst::Constructor(TypeAstConstructor {
                name,
                arguments: args,
                module,
                ..
            }) => self.type_ast_constructor(module, name, args),

            TypeAst::Fn(TypeAstFn {
                arguments: args,
                return_: retrn,
                ..
            }) => "fn"
                .to_doc()
                .append(self.type_arguments(args))
                .group()
                .append(" ->")
                .append(break_("", " ").append(self.type_ast(retrn)).nest(INDENT)),

            TypeAst::Var(TypeAstVar { name, .. }) => name.to_doc(),

            TypeAst::Tuple(TypeAstTuple { elems, .. }) => {
                "#".to_doc().append(self.type_arguments(elems))
            }
        }
        .group()
    }

    fn type_arguments<'a>(&mut self, args: &'a [TypeAst]) -> Document<'a> {
        wrap_args(args.iter().map(|t| self.type_ast(t)))
    }

    pub fn type_alias<'a>(
        &mut self,
        public: bool,
        name: &'a str,
        args: &'a [EcoString],
        typ: &'a TypeAst,
        deprecation: &'a Deprecation,
    ) -> Document<'a> {
        // @deprecated attribute
        let head = self.deprecation_attr(deprecation);

        let head = head.append(pub_(public)).append("type ").append(name);

        let head = if args.is_empty() {
            head
        } else {
            head.append(wrap_args(args.iter().map(|e| e.to_doc())).group())
        };

        head.append(" =")
            .append(line().append(self.type_ast(typ)).group().nest(INDENT))
    }

    fn deprecation_attr<'a>(&mut self, deprecation: &'a Deprecation) -> Document<'a> {
        match deprecation {
            Deprecation::NotDeprecated => "".to_doc(),
            Deprecation::Deprecated { message } => ""
                .to_doc()
                .append("@deprecated(\"")
                .append(message)
                .append("\")")
                .append(line()),
        }
    }

    fn fn_arg<'a, A>(&mut self, arg: &'a Arg<A>) -> Document<'a> {
        let comments = self.pop_comments(arg.location.start);
        let doc = match &arg.annotation {
            None => arg.names.to_doc(),
            Some(a) => arg.names.to_doc().append(": ").append(self.type_ast(a)),
        }
        .group();
        commented(doc, comments)
    }

    fn statement_fn<'a>(&mut self, function: &'a Function<(), UntypedExpr>) -> Document<'a> {
        // @deprecated attribute
        let attributes = self.deprecation_attr(&function.deprecation);

        // @external attribute
        let external = |t: &'static str, m: &'a str, f: &'a str| {
            docvec!["@external(", t, ", \"", m, "\", \"", f, "\")", line()]
        };
        let attributes = match function.external_erlang.as_ref() {
            Some((m, f)) => attributes.append(external("erlang", m, f)),
            None => attributes,
        };
        let attributes = match function.external_javascript.as_ref() {
            Some((m, f)) => attributes.append(external("javascript", m, f)),
            None => attributes,
        };

        // Fn name and args
        let signature = pub_(function.public)
            .append("fn ")
            .append(&function.name)
            .append(wrap_args(function.arguments.iter().map(|e| self.fn_arg(e))));

        // Add return annotation
        let signature = match &function.return_annotation {
            Some(anno) => signature.append(" -> ").append(self.type_ast(anno)),
            None => signature,
        }
        .group();

        let body = &function.body;
        if body.len() == 1 && body.first().is_placeholder() {
            return attributes.append(signature);
        }

        let head = attributes.append(signature);

        // Format body
        let body = self.statements(body);

        // Add any trailing comments
        let body = match printed_comments(self.pop_comments(function.end_position), false) {
            Some(comments) => body.append(line()).append(comments),
            None => body,
        };

        // Stick it all together
        head.append(" {")
            .append(line().append(body).nest(INDENT).group())
            .append(line())
            .append("}")
    }

    fn expr_fn<'a>(
        &mut self,
        args: &'a [UntypedArg],
        return_annotation: Option<&'a TypeAst>,
        body: &'a Vec1<UntypedStatement>,
    ) -> Document<'a> {
        let args = wrap_args(args.iter().map(|e| self.fn_arg(e)))
            .group()
            .next_break_fits(NextBreakFitsMode::Disabled);
        //   ^^^ We add this so that when an expression function is passed as
        //       the last argument of a function and it goes over the line
        //       limit with just its arguments we don't get some strange
        //       splitting.
        //       See https://github.com/gleam-lang/gleam/issues/2571
        //
        // There's many ways we could be smarter than this. For example:
        //  - still split the arguments like it did in the example shown in the
        //    issue if the expr_fn has more than one argument
        //  - make sure that an anonymous function whose body is made up of a
        //    single expression doesn't get split (I think that could boil down
        //    to wrapping the body with a `next_break_fits(Disabled)`)
        //
        // These are some of the ways we could tweak the look of expression
        // functions in the future if people are not satisfied with it.
        let body = self.statements(body);
        let header = "fn".to_doc().append(args);

        let header = match return_annotation {
            None => header,
            Some(t) => header.append(" -> ").append(self.type_ast(t)),
        };

        header.append(" ").append(wrap_block(body)).group()
    }

    fn statements<'a>(&mut self, statements: &'a Vec1<UntypedStatement>) -> Document<'a> {
        let mut previous_position = 0;
        let count = statements.len();
        let mut documents = Vec::with_capacity(count * 2);
        for (i, statement) in statements.iter().enumerate() {
            let preceding_newline = self.pop_empty_lines(previous_position + 1);
            if i != 0 && preceding_newline {
                documents.push(lines(2));
            } else if i != 0 {
                documents.push(lines(1));
            }
            previous_position = statement.location().end;
            documents.push(self.statement(statement).group());
        }
        if count == 1 && statements.first().is_expression() {
            documents.to_doc()
        } else {
            documents.to_doc().force_break()
        }
    }

    fn assignment<'a>(&mut self, assignment: &'a UntypedAssignment) -> Document<'a> {
        let comments = self.pop_comments(assignment.location.start);
        let Assignment {
            pattern,
            value,
            kind,
            annotation,
            ..
        } = assignment;

        let _ = self.pop_empty_lines(pattern.location().end);

        let keyword = match kind {
            AssignmentKind::Let => "let ",
            AssignmentKind::Assert => "let assert ",
        };

        let pattern = self.pattern(pattern);

        let annotation = annotation
            .as_ref()
            .map(|a| ": ".to_doc().append(self.type_ast(a)));

        let doc = keyword
            .to_doc()
            .append(pattern.append(annotation).group())
            .append(" =")
            .append(self.assigned_value(value));
        commented(doc, comments)
    }

    fn expr<'a>(&mut self, expr: &'a UntypedExpr) -> Document<'a> {
        let comments = self.pop_comments(expr.start_byte_index());

        let document = match expr {
            UntypedExpr::Placeholder { .. } => panic!("Placeholders should not be formatted"),

            UntypedExpr::Panic {
                message: Some(m), ..
            } => docvec!["panic as ", self.expr(m)],

            UntypedExpr::Panic { .. } => "panic".to_doc(),

            UntypedExpr::Todo { message: None, .. } => "todo".to_doc(),

            UntypedExpr::Todo {
                message: Some(l), ..
            } => docvec!["todo as ", self.expr(l)],

            UntypedExpr::PipeLine { expressions, .. } => self.pipeline(expressions),

            UntypedExpr::Int { value, .. } => self.int(value),

            UntypedExpr::Float { value, .. } => self.float(value),

            UntypedExpr::String { value, .. } => self.string(value),

            UntypedExpr::Block {
                statements,
                location,
                ..
            } => self.block(location, statements, false),

            UntypedExpr::Var { name, .. } if name == CAPTURE_VARIABLE => "_".to_doc(),

            UntypedExpr::Var { name, .. } => name.to_doc(),

            UntypedExpr::TupleIndex { tuple, index, .. } => self.tuple_index(tuple, *index),

            UntypedExpr::NegateInt { value, .. } => self.negate_int(value),

            UntypedExpr::NegateBool { value, .. } => self.negate_bool(value),

            UntypedExpr::Fn {
                is_capture: true,
                body,
                ..
            } => self.fn_capture(body),

            UntypedExpr::Fn {
                return_annotation,
                arguments: args,
                body,
                ..
            } => self.expr_fn(args, return_annotation.as_ref(), body),

            UntypedExpr::List {
                elements,
                tail,
                location,
            } => self.list(elements, tail.as_deref(), location),

            UntypedExpr::Call {
                fun,
                arguments: args,
                location,
                ..
            } => self.call(fun, args, location),

            UntypedExpr::BinOp {
                name, left, right, ..
            } => self.bin_op(name, left, right),

            UntypedExpr::Case {
                subjects,
                clauses,
                location,
            } => self.case(subjects, clauses, location),

            UntypedExpr::FieldAccess {
                label, container, ..
            } => if let UntypedExpr::TupleIndex { .. } = container.as_ref() {
                self.expr(container).surround("{ ", " }")
            } else {
                self.expr(container)
            }
            .append(".")
            .append(label.as_str()),

            UntypedExpr::Tuple { elems, location } => self.tuple(elems, location),

            UntypedExpr::BitArray { segments, .. } => bit_array(
                segments
                    .iter()
                    .map(|s| bit_array_segment(s, |e| self.bit_array_segment_expr(e))),
                segments.iter().all(|s| s.value.is_simple_constant()),
            ),
            UntypedExpr::RecordUpdate {
                constructor,
                spread,
                arguments: args,
                ..
            } => self.record_update(constructor, spread, args),
        };
        commented(document, comments)
    }

    fn string<'a>(&self, string: &'a EcoString) -> Document<'a> {
        let doc = string.to_doc().surround("\"", "\"");
        if string.contains('\n') {
            doc.force_break()
        } else {
            doc
        }
    }

    fn bin_op_string<'a>(&self, string: &'a EcoString) -> Document<'a> {
        let lines = string.split('\n').collect_vec();
        match lines.as_slice() {
            [] | [_] => string.to_doc().surround("\"", "\""),
            [first_line, lines @ ..] => {
                let mut doc = docvec!("\"", first_line);
                for line in lines {
                    doc = doc
                        .append(pretty::line().set_nesting(0))
                        .append(line.to_doc())
                }
                doc.append("\"".to_doc()).group()
            }
        }
    }

    fn float<'a>(&self, value: &'a str) -> Document<'a> {
        // Create parts
        let mut parts = value.split('.');
        let integer_part = parts.next().unwrap_or_default();
        // floating point part
        let fp_part = parts.next().unwrap_or_default();
        let integer_doc = self.underscore_integer_string(integer_part);
        let dot_doc = ".".to_doc();

        // Split fp_part into a regular fractional and maybe a scientific part
        let (fp_part_fractional, fp_part_scientific) = fp_part.split_at(
            fp_part
                .chars()
                .position(|ch| ch == 'e')
                .unwrap_or(fp_part.len()),
        );

        // Trim right any consequtive '0's
        let mut fp_part_fractional = fp_part_fractional.trim_end_matches('0').to_string();
        // If there is no fractional part left, add a '0', thus that 1. becomes 1.0 etc.
        if fp_part_fractional.is_empty() {
            fp_part_fractional.push('0');
        }
        let fp_doc = fp_part_fractional.chars().collect::<EcoString>();

        integer_doc
            .append(dot_doc)
            .append(fp_doc)
            .append(fp_part_scientific)
    }

    fn int<'a>(&self, value: &'a str) -> Document<'a> {
        if value.starts_with("0x") || value.starts_with("0b") || value.starts_with("0o") {
            return value.to_doc();
        }

        self.underscore_integer_string(value)
    }

    fn underscore_integer_string<'a>(&self, value: &'a str) -> Document<'a> {
        let underscore_ch = '_';
        let minus_ch = '-';

        let len = value.len();
        let underscore_ch_cnt = value.matches(underscore_ch).count();
        let reformat_watershed = if value.starts_with(minus_ch) { 6 } else { 5 };
        let insert_underscores = (len - underscore_ch_cnt) >= reformat_watershed;

        let mut new_value = String::new();
        let mut j = 0;
        for (i, ch) in value.chars().rev().enumerate() {
            if ch == '_' {
                continue;
            }

            if insert_underscores && i != 0 && ch != minus_ch && i < len && j % 3 == 0 {
                new_value.push(underscore_ch);
            }
            new_value.push(ch);

            j += 1;
        }

        new_value.chars().rev().collect::<EcoString>().to_doc()
    }

    fn pattern_constructor<'a>(
        &mut self,
        name: &'a str,
        args: &'a [CallArg<UntypedPattern>],
        module: &'a Option<EcoString>,
        with_spread: bool,
    ) -> Document<'a> {
        fn is_breakable(expr: &UntypedPattern) -> bool {
            match expr {
                Pattern::Tuple { .. } | Pattern::List { .. } | Pattern::BitArray { .. } => true,
                Pattern::Constructor {
                    arguments: args, ..
                } => !args.is_empty(),
                _ => false,
            }
        }

        let name = match module {
            Some(m) => m.to_doc().append(".").append(name),
            None => name.to_doc(),
        };

        if args.is_empty() && with_spread {
            name.append("(..)")
        } else if args.is_empty() {
            name
        } else if with_spread {
            name.append(wrap_args_with_spread(
                args.iter().map(|a| self.pattern_call_arg(a)),
            ))
        } else {
            match args {
                [arg] if is_breakable(&arg.value) => name
                    .append("(")
                    .append(self.pattern_call_arg(arg))
                    .append(")")
                    .group(),

                _ => name
                    .append(wrap_args(args.iter().map(|a| self.pattern_call_arg(a))))
                    .group(),
            }
        }
    }

    fn call<'a>(
        &mut self,
        fun: &'a UntypedExpr,
        args: &'a [CallArg<UntypedExpr>],
        location: &SrcSpan,
    ) -> Document<'a> {
        let expr = match fun {
            UntypedExpr::Placeholder { .. } => panic!("Placeholders should not be formatted"),

            UntypedExpr::PipeLine { .. } => break_block(self.expr(fun)),

            UntypedExpr::BinOp { .. }
            | UntypedExpr::Int { .. }
            | UntypedExpr::Float { .. }
            | UntypedExpr::String { .. }
            | UntypedExpr::Block { .. }
            | UntypedExpr::Var { .. }
            | UntypedExpr::Fn { .. }
            | UntypedExpr::List { .. }
            | UntypedExpr::Call { .. }
            | UntypedExpr::Case { .. }
            | UntypedExpr::FieldAccess { .. }
            | UntypedExpr::Tuple { .. }
            | UntypedExpr::TupleIndex { .. }
            | UntypedExpr::Todo { .. }
            | UntypedExpr::Panic { .. }
            | UntypedExpr::BitArray { .. }
            | UntypedExpr::RecordUpdate { .. }
            | UntypedExpr::NegateBool { .. }
            | UntypedExpr::NegateInt { .. } => self.expr(fun),
        };

        let arity = args.len();
        self.append_inlinable_wrapped_args(
            expr,
            args,
            location,
            |arg| &arg.value,
            |self_, arg| self_.call_arg(arg, arity),
        )
    }

    fn tuple<'a>(&mut self, elements: &'a [UntypedExpr], location: &SrcSpan) -> Document<'a> {
        self.append_inlinable_wrapped_args(
            "#".to_doc(),
            elements,
            location,
            |e| e,
            |self_, e| {
                // If there's more than one item in the tuple and there's a
                // pipeline or long binary chain, we want to indent those to
                // make it easier to tell where one item ends and the other
                // starts.
                if elements.len() > 1 && (e.is_binop() || e.is_pipeline()) {
                    self_.expr(e).group().nest(INDENT)
                } else {
                    self_.expr(e).group()
                }
            },
        )
    }

    // Appends to the given docs a comma-separated list of documents wrapped by
    // parentheses. If the last item of the argument list is splittable the
    // resulting document will try to first split that before splitting all the
    // other arguments.
    // This is used for function calls and tuples.
    fn append_inlinable_wrapped_args<'a, T, ToExpr, ToDoc>(
        &mut self,
        doc: Document<'a>,
        values: &'a [T],
        location: &SrcSpan,
        to_expr: ToExpr,
        to_doc: ToDoc,
    ) -> Document<'a>
    where
        T: HasLocation,
        ToExpr: Fn(&T) -> &UntypedExpr,
        ToDoc: Fn(&mut Self, &'a T) -> Document<'a>,
    {
        match init_and_last(values) {
            Some((initial_values, last_value))
                if is_breakable_argument(to_expr(last_value), values.len())
                    && !self.any_comments(last_value.location().start) =>
            {
                let last_value_doc = to_doc(self, last_value)
                    .group()
                    .next_break_fits(NextBreakFitsMode::Enabled);

                let docs = initial_values
                    .iter()
                    .map(|value| to_doc(self, value))
                    .chain(std::iter::once(last_value_doc))
                    .collect_vec();

                doc.append(self.wrap_function_call_args(docs, location))
                    .next_break_fits(NextBreakFitsMode::Disabled)
                    .group()
            }

            Some(_) | None => {
                let docs = values.iter().map(|value| to_doc(self, value)).collect_vec();
                doc.append(self.wrap_function_call_args(docs, location))
                    .group()
            }
        }
    }

    pub fn case<'a>(
        &mut self,
        subjects: &'a [UntypedExpr],
        clauses: &'a [UntypedClause],
        location: &'a SrcSpan,
    ) -> Document<'a> {
        let subjects_doc = break_("case", "case ")
            .append(join(
                subjects.iter().map(|s| self.expr(s)),
                break_(",", ", "),
            ))
            .nest(INDENT)
            .append(break_("", " "))
            .append("{")
            .group();

        let clauses_doc = concat(
            clauses
                .iter()
                .enumerate()
                .map(|(i, c)| self.clause(c, i as u32)),
        );

        // We get all remaining comments that come before the case's closing
        // bracket. If there's any we add those before the closing bracket
        // instead of moving those out of the case expression.
        // Otherwise those would be moved out of the case expression.
        let comments = self.pop_comments(location.end);
        let closing_bracket = match printed_comments(comments, false) {
            None => docvec!(line(), "}"),
            Some(comment) => docvec!(line().nest(INDENT), comment, line(), "}"),
        };

        subjects_doc
            .append(line().append(clauses_doc).nest(INDENT))
            .append(closing_bracket)
            .force_break()
    }

    pub fn record_update<'a>(
        &mut self,
        constructor: &'a UntypedExpr,
        spread: &'a RecordUpdateSpread,
        args: &'a [UntypedRecordUpdateArg],
    ) -> Document<'a> {
        use std::iter::once;
        let constructor_doc = self.expr(constructor);
        let comments = self.pop_comments(spread.base.location().start);
        let spread_doc = commented("..".to_doc().append(self.expr(&spread.base)), comments);
        let arg_docs = args.iter().map(|a| self.record_update_arg(a));
        let all_arg_docs = once(spread_doc).chain(arg_docs);
        constructor_doc.append(wrap_args(all_arg_docs)).group()
    }

    pub fn bin_op<'a>(
        &mut self,
        name: &'a BinOp,
        left: &'a UntypedExpr,
        right: &'a UntypedExpr,
    ) -> Document<'a> {
        self.bin_op_side(name, left)
            .append(break_("", " "))
            .append(name)
            .append(" ")
            .append(self.bin_op_side(name, right))
    }

    fn bin_op_side<'a>(&mut self, operator: &'a BinOp, side: &'a UntypedExpr) -> Document<'a> {
        let side_doc = match side {
            UntypedExpr::String { value, .. } => self.bin_op_string(value),
            _ => self.expr(side),
        };
        match side.bin_op_name() {
            // In case the other side is a binary operation as well and it can
            // be grouped together with the current binary operation, the two
            // docs are simply concatenated, so that they will end up in the
            // same group and the formatter will try to keep those on a single
            // line.
            Some(side_name) if side_name.can_be_grouped_with(operator) => side_doc,
            // In case the binary operations cannot be grouped together the
            // other side is treated as a group on its own so that it can be
            // broken independently of other pieces of the binary operations
            // chain.
            _ => self.operator_side(
                side_doc.group(),
                operator.precedence(),
                side.bin_op_precedence(),
            ),
        }
    }

    pub fn operator_side<'a>(&self, doc: Document<'a>, op: u8, side: u8) -> Document<'a> {
        if op > side {
            wrap_block(doc).group()
        } else {
            doc
        }
    }

    fn pipeline<'a>(&mut self, expressions: &'a Vec1<UntypedExpr>) -> Document<'a> {
        let mut docs = Vec::with_capacity(expressions.len() * 3);
        let first = expressions.first();
        let first_precedence = first.bin_op_precedence();
        let first = self.expr(first).group();
        docs.push(self.operator_side(first, 5, first_precedence));

        for expr in expressions.iter().skip(1) {
            let comments = self.pop_comments(expr.location().start);
            let doc = match expr {
                UntypedExpr::Fn {
                    is_capture: true,
                    body,
                    ..
                } => {
                    let body = match body.first() {
                        Statement::Expression(expression) => expression,
                        Statement::Assignment(_) | Statement::Use(_) => {
                            unreachable!("Non expression capture body")
                        }
                    };
                    self.pipe_capture_right_hand_side(body)
                }

                _ => self.expr(expr),
            };
            docs.push(line());
            docs.push(commented("|> ".to_doc(), comments));
            docs.push(self.operator_side(doc, 4, expr.bin_op_precedence()));
        }

        docs.to_doc().force_break()
    }

    fn pipe_capture_right_hand_side<'a>(&mut self, fun: &'a UntypedExpr) -> Document<'a> {
        let (fun, args) = match fun {
            UntypedExpr::Call {
                fun,
                arguments: args,
                ..
            } => (fun, args),
            _ => panic!("Function capture found not to have a function call body when formatting"),
        };

        let hole_in_first_position = matches!(
            args.first(),
            Some(CallArg {
                value: UntypedExpr::Var { name, .. },
                ..
            }) if name == CAPTURE_VARIABLE
        );
        let arity = args.len();

        if hole_in_first_position && args.len() == 1 {
            // x |> fun(_)
            self.expr(fun)
        } else if hole_in_first_position {
            // x |> fun(_, 2, 3)
            self.expr(fun)
                .append(wrap_args(args.iter().skip(1).map(|a| self.call_arg(a, arity))).group())
        } else {
            // x |> fun(1, _, 3)
            self.expr(fun)
                .append(wrap_args(args.iter().map(|a| self.call_arg(a, arity))).group())
        }
    }

    fn fn_capture<'a>(&mut self, call: &'a [UntypedStatement]) -> Document<'a> {
        // The body of a capture being multiple statements shouldn't be possible...
        if call.len() != 1 {
            panic!("Function capture found not to have a single statement call");
        }

        match call.first() {
            Some(Statement::Expression(UntypedExpr::Call {
                fun,
                arguments: args,
                ..
            })) => {
                let arity = args.len();
                match args.as_slice() {
                    [first, second]
                        if is_breakable_expr(&second.value) && first.is_capture_hole() =>
                    {
                        self.expr(fun)
                            .append("(_, ")
                            .append(self.call_arg(second, arity))
                            .append(")")
                            .group()
                    }

                    _ => self
                        .expr(fun)
                        .append(wrap_args(args.iter().map(|a| self.call_arg(a, arity))).group()),
                }
            }

            // The body of a capture being not a fn shouldn't be possible...
            _ => panic!("Function capture body found not to be a call in the formatter"),
        }
    }

    pub fn record_constructor<'a, A>(
        &mut self,
        constructor: &'a RecordConstructor<A>,
    ) -> Document<'a> {
        let comments = self.pop_comments(constructor.location.start);
        let doc_comments = self.doc_comments(constructor.location.start);

        let doc = if constructor.arguments.is_empty() {
            constructor.name.as_str().to_doc()
        } else {
            constructor
                .name
                .as_str()
                .to_doc()
                .append(wrap_args(constructor.arguments.iter().map(
                    |RecordConstructorArg {
                         label,
                         ast,
                         location,
                         ..
                     }| {
                        let arg_comments = self.pop_comments(location.start);
                        let arg = match label {
                            Some(l) => l.to_doc().append(": ").append(self.type_ast(ast)),
                            None => self.type_ast(ast),
                        };

                        commented(
                            self.doc_comments(location.start).append(arg).group(),
                            arg_comments,
                        )
                    },
                )))
                .group()
        };

        commented(doc_comments.append(doc).group(), comments)
    }

    pub fn custom_type<'a, A>(&mut self, ct: &'a CustomType<A>) -> Document<'a> {
        let _ = self.pop_empty_lines(ct.location.end);

        // @deprecated attribute
        let doc = self.deprecation_attr(&ct.deprecation);

        let doc = doc
            .append(pub_(ct.public))
            .to_doc()
            .append(if ct.opaque { "opaque type " } else { "type " })
            .append(if ct.parameters.is_empty() {
                Document::EcoString(ct.name.clone())
            } else {
                Document::EcoString(ct.name.clone())
                    .append(wrap_args(ct.parameters.iter().map(|e| e.to_doc())))
                    .group()
            });

        if ct.constructors.is_empty() {
            return doc;
        }
        let doc = doc.append(" {");

        let inner = concat(ct.constructors.iter().map(|c| {
            if self.pop_empty_lines(c.location.start) {
                lines(2)
            } else {
                line()
            }
            .append(self.record_constructor(c))
        }));

        // Add any trailing comments
        let inner = match printed_comments(self.pop_comments(ct.end_position), false) {
            Some(comments) => inner.append(line()).append(comments),
            None => inner,
        }
        .nest(INDENT)
        .group();

        doc.append(inner).append(line()).append("}")
    }

    pub fn docs_opaque_custom_type<'a>(
        &mut self,
        public: bool,
        name: &'a str,
        args: &'a [EcoString],
        location: &'a SrcSpan,
    ) -> Document<'a> {
        let _ = self.pop_empty_lines(location.start);
        pub_(public)
            .to_doc()
            .append("opaque type ")
            .append(if args.is_empty() {
                name.to_doc()
            } else {
                name.to_doc()
                    .append(wrap_args(args.iter().map(|e| e.to_doc())))
            })
    }

    pub fn docs_fn_signature<'a>(
        &mut self,
        public: bool,
        name: &'a str,
        args: &'a [TypedArg],
        return_type: Arc<Type>,
    ) -> Document<'a> {
        let mut printer = type_::pretty::Printer::new();

        pub_(public)
            .append("fn ")
            .append(name)
            .append(self.docs_fn_args(args, &mut printer))
            .append(" -> ".to_doc())
            .append(printer.print(&return_type))
    }

    // Will always print the types, even if they were implicit in the original source
    fn docs_fn_args<'a>(
        &mut self,
        args: &'a [TypedArg],
        printer: &mut type_::pretty::Printer,
    ) -> Document<'a> {
        wrap_args(args.iter().map(|arg| {
            self.docs_fn_arg_name(arg)
                .append(": ".to_doc().append(printer.print(&arg.type_)))
                .group()
        }))
    }

    fn docs_fn_arg_name<'a>(&mut self, arg: &'a TypedArg) -> Document<'a> {
        match &arg.names {
            ArgNames::Named { name } => name.to_doc(),
            ArgNames::NamedLabelled { label, name } => docvec![label, " ", name],
            // We remove the underscore from discarded function arguments since we don't want to
            // expose this kind of detail: https://github.com/gleam-lang/gleam/issues/2561
            ArgNames::Discard { name } => name.strip_prefix('_').unwrap_or(name).to_doc(),
            ArgNames::LabelledDiscard { label, name } => {
                docvec![label, " ", name.strip_prefix('_').unwrap_or(name).to_doc()]
            }
        }
    }

    fn call_arg<'a>(&mut self, arg: &'a CallArg<UntypedExpr>, arity: usize) -> Document<'a> {
        match &arg.label {
            Some(s) => commented(
                s.to_doc().append(": "),
                self.pop_comments(arg.location.start),
            ),
            None => nil(),
        }
        .append(
            // If there's more than one item in the tuple and there's a
            // pipeline or long binary chain, we want to indent those to
            // make it easier to tell where one item ends and the other
            // starts.
            if arity > 1 && (arg.value.is_binop() || arg.value.is_pipeline()) {
                self.expr(&arg.value).group().nest(INDENT)
            } else {
                self.expr(&arg.value).group()
            },
        )
    }

    fn record_update_arg<'a>(&mut self, arg: &'a UntypedRecordUpdateArg) -> Document<'a> {
        let comments = self.pop_comments(arg.location.start);
        let doc = arg
            .label
            .as_str()
            .to_doc()
            .append(": ")
            .append(self.expr(&arg.value));

        if arg.value.is_binop() || arg.value.is_pipeline() {
            commented(doc, comments).nest(INDENT)
        } else {
            commented(doc, comments)
        }
    }

    fn tuple_index<'a>(&mut self, tuple: &'a UntypedExpr, index: u64) -> Document<'a> {
        match tuple {
            UntypedExpr::TupleIndex { .. } => self.expr(tuple).surround("{", "}"),
            _ => self.expr(tuple),
        }
        .append(".")
        .append(index)
    }

    fn case_clause_value<'a>(&mut self, expr: &'a UntypedExpr) -> Document<'a> {
        match expr {
            UntypedExpr::Fn { .. }
            | UntypedExpr::List { .. }
            | UntypedExpr::Tuple { .. }
            | UntypedExpr::BitArray { .. } => " ".to_doc().append(self.expr(expr)),

            UntypedExpr::Case { .. } => line().append(self.expr(expr)).nest(INDENT),

            UntypedExpr::Block {
                statements,
                location,
                ..
            } => " ".to_doc().append(self.block(location, statements, true)),

            _ => break_("", " ").append(self.expr(expr).group()).nest(INDENT),
        }
        .next_break_fits(NextBreakFitsMode::Disabled)
        .group()
    }

    fn assigned_value<'a>(&mut self, expr: &'a UntypedExpr) -> Document<'a> {
        match expr {
            UntypedExpr::Case { .. } => " ".to_doc().append(self.expr(expr)).group(),
            _ => self.case_clause_value(expr),
        }
    }

    fn clause<'a>(&mut self, clause: &'a UntypedClause, index: u32) -> Document<'a> {
        let space_before = self.pop_empty_lines(clause.location.start);
        let comments = self.pop_comments(clause.location.start);

        let clause_doc = match &clause.guard {
            None => self.alternative_patterns(clause),
            Some(guard) => self
                .alternative_patterns(clause)
                .append(break_("", " ").nest(INDENT))
                .append("if ")
                .append(self.clause_guard(guard).group().nest(INDENT)),
        };

        let clause_doc = match printed_comments(comments, false) {
            Some(comments) => comments.append(line()).append(clause_doc),
            None => clause_doc,
        };

        // In case there's a guard or multiple subjects, if we decide to break
        // the patterns on multiple lines we also want the arrow to end up on
        // its own line to improve legibility.
        //
        // This looks like this:
        // ```gleam
        // case wibble, wobble {
        //   Wibble(_),  // pretend this goes over the line limit
        //     Wobble(_)
        //   -> todo
        //   // Notice how the arrow is broken on its own line, the same goes
        //   // for patterns with `if` guards.
        // }
        // ```
        let has_guard = clause.guard.is_some();
        let has_multiple_subjects = clause.pattern.len() > 1;
        let arrow_break = if has_guard || has_multiple_subjects {
            break_("", " ")
        } else {
            " ".to_doc()
        };

        if index == 0 {
            clause_doc
        } else if space_before {
            lines(2).append(clause_doc)
        } else {
            lines(1).append(clause_doc)
        }
        .append(arrow_break)
        .group()
        .append("->")
        .append(self.case_clause_value(&clause.then))
    }

    fn alternative_patterns<'a>(&mut self, clause: &'a UntypedClause) -> Document<'a> {
        let has_guard = clause.guard.is_some();
        let has_multiple_subjects = clause.pattern.len() > 1;

        // In case there's an `if` guard but no multiple subjects we want to add
        // additional indentation before the vartical bar separating alternative
        // patterns `|`.
        // We're not adding the indentation if there's multiple subjects as that
        // would make things harder to read, aligning the vertical bar with the
        // different subjects:
        // ```
        // case wibble, wobble {
        //   Wibble,
        //     Wobble
        //     | Wibble, // <- we don't want this indentation!
        //     Wobble -> todo
        // }
        // ```
        let alternatives_separator = if has_guard && !has_multiple_subjects {
            break_("", " ").nest(INDENT).append("| ")
        } else {
            break_("", " ").append("| ")
        };

        let alternative_patterns = std::iter::once(&clause.pattern)
            .chain(&clause.alternative_patterns)
            .map(|p| {
                // Here `p` is a single pattern that can be comprised of
                // multiple subjects.
                // ```gleam
                // case wibble, wobble {
                //   True, False
                // //^^^^^^^^^^^ This is a single pattern with multiple subjects
                //   | _, _ -> todo
                // }
                // ```
                //
                // We turn each subject pattern into a document and join those
                // with a breakable comma (that's also going to be nested).
                // Then we make sure that the formatter tries to keep each
                // alternative on a single line by making it a group!
                let patterns = p.iter().map(|p| self.pattern(p));
                join(patterns, break_(",", ", ")).group().nest(INDENT)
            });

        join(alternative_patterns, alternatives_separator).group()
    }

    fn list<'a>(
        &mut self,
        elements: &'a [UntypedExpr],
        tail: Option<&'a UntypedExpr>,
        location: &SrcSpan,
    ) -> Document<'a> {
        if elements.is_empty() {
            return match tail {
                Some(tail) => self.expr(tail),
                None => "[]".to_doc(),
            };
        }

        let comma = if tail.is_none() && elements.iter().all(UntypedExpr::is_simple_constant) {
            flex_break(",", ", ")
        } else {
            break_(",", ", ")
        };

        let list_size = elements.len()
            + match tail {
                Some(_) => 1,
                None => 0,
            };

        let elements = join(
            elements.iter().map(|e|
                // If there's more than one item in the tuple and there's a
                // pipeline or long binary chain, we want to indent those to
                // make it easier to tell where one item ends and the other
                // starts.
                if list_size > 1 && (e.is_binop() || e.is_pipeline()) {
                    self.expr(e).group().nest(INDENT)
                } else {
                    self.expr(e).group()
                }
            ),
            comma,
        )
        .next_break_fits(NextBreakFitsMode::Disabled);

        let doc = break_("[", "[").append(elements);
        // We need to keep the last break aside and do not add it immediately
        // because in case there's a final comment before the closing square
        // bracket we want to add indentation (to just that break). Otherwise,
        // the final comment would be less indented than list's elements.
        let (doc, last_break) = match tail {
            None => (doc.nest(INDENT), break_(",", "")),

            Some(tail) => {
                let comments = self.pop_comments(tail.location().start);
                let tail = commented(docvec!["..", self.expr(tail)], comments);
                (
                    doc.append(break_(",", ", ")).append(tail).nest(INDENT),
                    break_("", ""),
                )
            }
        };

        // We get all remaining comments that come before the list's closing
        // square bracket.
        // If there's any we add those before the closing square bracket instead
        // of moving those out of the list.
        // Otherwise those would be moved out of the list.
        let comments = self.pop_comments(location.end);
        match printed_comments(comments, false) {
            None => doc.append(last_break).append("]").group(),
            Some(comment) => doc
                .append(last_break.nest(INDENT))
                // ^ See how here we're adding the missing indentation to the
                //   final break so that the final comment is as indented as the
                //   list's items.
                .append(comment)
                .append(line())
                .append("]")
                .force_break(),
        }
    }

    fn pattern<'a>(&mut self, pattern: &'a UntypedPattern) -> Document<'a> {
        let comments = self.pop_comments(pattern.location().start);
        let doc = match pattern {
            Pattern::Int { value, .. } => self.int(value),

            Pattern::Float { value, .. } => self.float(value),

            Pattern::String { value, .. } => self.string(value),

            Pattern::Variable { name, .. } => name.to_doc(),

            Pattern::VarUsage { name, .. } => name.to_doc(),

            Pattern::Assign { name, pattern, .. } => {
                self.pattern(pattern).append(" as ").append(name.as_str())
            }

            Pattern::Discard { name, .. } => name.to_doc(),

            Pattern::List { elements, tail, .. } => self.list_pattern(elements, tail),

            Pattern::Constructor {
                name,
                arguments: args,
                module,
                with_spread,
                ..
            } => self.pattern_constructor(name, args, module, *with_spread),

            Pattern::Tuple { elems, .. } => "#"
                .to_doc()
                .append(wrap_args(elems.iter().map(|e| self.pattern(e))))
                .group(),

            Pattern::BitArray { segments, .. } => bit_array(
                segments
                    .iter()
                    .map(|s| bit_array_segment(s, |e| self.pattern(e))),
                false,
            ),

            Pattern::StringPrefix {
                left_side_string: left,
                right_side_assignment: right,
                left_side_assignment: left_assign,
                ..
            } => {
                let left = self.string(left);
                let right = match right {
                    AssignName::Variable(name) => name.to_doc(),
                    AssignName::Discard(name) => name.to_doc(),
                };
                match left_assign {
                    Some((name, _)) => docvec![left, " as ", name, " <> ", right],
                    None => docvec![left, " <> ", right],
                }
            }
        };
        commented(doc, comments)
    }

    fn list_pattern<'a>(
        &mut self,
        elements: &'a [Pattern<()>],
        tail: &'a Option<Box<Pattern<()>>>,
    ) -> Document<'a> {
        if elements.is_empty() {
            return match tail {
                Some(tail) => self.pattern(tail),
                None => "[]".to_doc(),
            };
        }
        let elements = join(elements.iter().map(|e| self.pattern(e)), break_(",", ", "));
        let doc = break_("[", "[").append(elements);
        match tail {
            None => doc.nest(INDENT).append(break_(",", "")),

            Some(tail) => {
                let comments = self.pop_comments(tail.location().start);
                let tail = if tail.is_discard() {
                    "..".to_doc()
                } else {
                    docvec!["..", self.pattern(tail)]
                };
                let tail = commented(tail, comments);
                doc.append(break_(",", ", "))
                    .append(tail)
                    .nest(INDENT)
                    .append(break_("", ""))
            }
        }
        .append("]")
        .group()
    }

    fn pattern_call_arg<'a>(&mut self, arg: &'a CallArg<UntypedPattern>) -> Document<'a> {
        arg.label
            .as_ref()
            .map(|s| s.to_doc().append(": "))
            .unwrap_or_else(nil)
            .append(self.pattern(&arg.value))
    }

    pub fn clause_guard_bin_op<'a>(
        &mut self,
        name: &'a BinOp,
        left: &'a UntypedClauseGuard,
        right: &'a UntypedClauseGuard,
    ) -> Document<'a> {
        self.clause_guard_bin_op_side(name, left, left.precedence())
            .append(break_("", " "))
            .append(name.to_doc())
            .append(" ")
            .append(self.clause_guard_bin_op_side(name, right, right.precedence() - 1))
    }

    fn clause_guard_bin_op_side<'a>(
        &mut self,
        name: &BinOp,
        side: &'a UntypedClauseGuard,
        // As opposed to `bin_op_side`, here we take the side precedence as an
        // argument instead of computing it ourselves. That's because
        // `clause_guard_bin_op` will reduce the precedence of any right side to
        // make sure the formatter doesn't remove any needed curly bracket.
        side_precedence: u8,
    ) -> Document<'a> {
        let side_doc = self.clause_guard(side);
        match side.bin_op_name() {
            // In case the other side is a binary operation as well and it can
            // be grouped together with the current binary operation, the two
            // docs are simply concatenated, so that they will end up in the
            // same group and the formatter will try to keep those on a single
            // line.
            Some(side_name) if side_name.can_be_grouped_with(name) => {
                self.operator_side(side_doc, name.precedence(), side_precedence)
            }
            // In case the binary operations cannot be grouped together the
            // other side is treated as a group on its own so that it can be
            // broken independently of other pieces of the binary operations
            // chain.
            _ => self.operator_side(side_doc.group(), name.precedence(), side_precedence),
        }
    }

    fn clause_guard<'a>(&mut self, clause_guard: &'a UntypedClauseGuard) -> Document<'a> {
        match clause_guard {
            ClauseGuard::And { left, right, .. } => {
                self.clause_guard_bin_op(&BinOp::And, left, right)
            }
            ClauseGuard::Or { left, right, .. } => {
                self.clause_guard_bin_op(&BinOp::Or, left, right)
            }
            ClauseGuard::Equals { left, right, .. } => {
                self.clause_guard_bin_op(&BinOp::Eq, left, right)
            }
            ClauseGuard::NotEquals { left, right, .. } => {
                self.clause_guard_bin_op(&BinOp::NotEq, left, right)
            }
            ClauseGuard::GtInt { left, right, .. } => {
                self.clause_guard_bin_op(&BinOp::GtInt, left, right)
            }
            ClauseGuard::GtEqInt { left, right, .. } => {
                self.clause_guard_bin_op(&BinOp::GtEqInt, left, right)
            }
            ClauseGuard::LtInt { left, right, .. } => {
                self.clause_guard_bin_op(&BinOp::LtInt, left, right)
            }
            ClauseGuard::LtEqInt { left, right, .. } => {
                self.clause_guard_bin_op(&BinOp::LtEqInt, left, right)
            }
            ClauseGuard::GtFloat { left, right, .. } => {
                self.clause_guard_bin_op(&BinOp::GtFloat, left, right)
            }
            ClauseGuard::GtEqFloat { left, right, .. } => {
                self.clause_guard_bin_op(&BinOp::GtEqFloat, left, right)
            }
            ClauseGuard::LtFloat { left, right, .. } => {
                self.clause_guard_bin_op(&BinOp::LtFloat, left, right)
            }
            ClauseGuard::LtEqFloat { left, right, .. } => {
                self.clause_guard_bin_op(&BinOp::LtEqFloat, left, right)
            }

            ClauseGuard::Var { name, .. } => name.to_doc(),

            ClauseGuard::TupleIndex { tuple, index, .. } => {
                self.clause_guard(tuple).append(".").append(*index).to_doc()
            }

            ClauseGuard::FieldAccess {
                container, label, ..
            } => self
                .clause_guard(container)
                .append(".")
                .append(label)
                .to_doc(),

            ClauseGuard::ModuleSelect {
                module_name, label, ..
            } => module_name.to_doc().append(".").append(label).to_doc(),

            ClauseGuard::Constant(constant) => self.const_expr(constant),

            ClauseGuard::Not { expression, .. } => docvec!["!", self.clause_guard(expression)],
        }
    }

    fn constant_call_arg<'a, A, B>(&mut self, arg: &'a CallArg<Constant<A, B>>) -> Document<'a> {
        match &arg.label {
            None => self.const_expr(&arg.value),
            Some(s) => s.to_doc().append(": ").append(self.const_expr(&arg.value)),
        }
    }

    fn negate_bool<'a>(&mut self, expr: &'a UntypedExpr) -> Document<'a> {
        match expr {
            UntypedExpr::BinOp { .. } => "!".to_doc().append(wrap_block(self.expr(expr))),
            _ => docvec!["!", self.expr(expr)],
        }
    }

    fn negate_int<'a>(&mut self, expr: &'a UntypedExpr) -> Document<'a> {
        match expr {
            UntypedExpr::BinOp { .. } | UntypedExpr::NegateInt { .. } => {
                "- ".to_doc().append(self.expr(expr))
            }

            _ => docvec!["-", self.expr(expr)],
        }
    }

    fn use_<'a>(&mut self, use_: &'a Use) -> Document<'a> {
        let comments = self.pop_comments(use_.location.start);

        let call = if use_.call.is_call() {
            docvec![" ", self.expr(&use_.call)]
        } else {
            docvec![break_("", " "), self.expr(&use_.call)].nest(INDENT)
        }
        .group();

        let doc = if use_.assignments.is_empty() {
            docvec!["use <-", call]
        } else {
            let assignments = use_.assignments.iter().map(|use_assignment| {
                let pattern = self.pattern(&use_assignment.pattern);
                let annotation = use_assignment
                    .annotation
                    .as_ref()
                    .map(|a| ": ".to_doc().append(self.type_ast(a)));

                pattern.append(annotation).group()
            });
            let assignments = Itertools::intersperse(assignments, break_(",", ", "));
            let left = ["use".to_doc(), break_("", " ")]
                .into_iter()
                .chain(assignments);
            let left = concat(left).nest(INDENT).append(break_("", " ")).group();
            docvec![left, "<-", call].group()
        };

        commented(doc, comments)
    }

    fn bit_array_segment_expr<'a>(&mut self, expr: &'a UntypedExpr) -> Document<'a> {
        match expr {
            UntypedExpr::Placeholder { .. } => panic!("Placeholders should not be formatted"),

            UntypedExpr::BinOp { .. } => wrap_block(self.expr(expr)),

            UntypedExpr::Int { .. }
            | UntypedExpr::Float { .. }
            | UntypedExpr::String { .. }
            | UntypedExpr::Var { .. }
            | UntypedExpr::Fn { .. }
            | UntypedExpr::List { .. }
            | UntypedExpr::Call { .. }
            | UntypedExpr::PipeLine { .. }
            | UntypedExpr::Case { .. }
            | UntypedExpr::FieldAccess { .. }
            | UntypedExpr::Tuple { .. }
            | UntypedExpr::TupleIndex { .. }
            | UntypedExpr::Todo { .. }
            | UntypedExpr::Panic { .. }
            | UntypedExpr::BitArray { .. }
            | UntypedExpr::RecordUpdate { .. }
            | UntypedExpr::NegateBool { .. }
            | UntypedExpr::NegateInt { .. }
            | UntypedExpr::Block { .. } => self.expr(expr),
        }
    }

    fn statement<'a>(&mut self, statement: &'a Statement<(), UntypedExpr>) -> Document<'a> {
        match statement {
            Statement::Expression(expression) => self.expr(expression),
            Statement::Assignment(assignment) => self.assignment(assignment),
            Statement::Use(use_) => self.use_(use_),
        }
    }

    fn block<'a>(
        &mut self,
        location: &SrcSpan,
        statements: &'a Vec1<UntypedStatement>,
        force_breaks: bool,
    ) -> Document<'a> {
        let statements_doc = docvec![break_("", " "), self.statements(statements)].nest(INDENT);
        let trailing_comments = self.pop_comments(location.end);
        let trailing_comments = printed_comments(trailing_comments, false);
        let block_doc = match trailing_comments {
            Some(trailing_comments_doc) => docvec![
                "{",
                statements_doc,
                line().nest(INDENT),
                trailing_comments_doc.nest(INDENT),
                line(),
                "}"
            ]
            .force_break(),
            None => docvec!["{", statements_doc, break_("", " "), "}"],
        };

        if force_breaks {
            block_doc.force_break().group()
        } else {
            block_doc.group()
        }
    }

    pub fn wrap_function_call_args<'a, I>(&mut self, args: I, location: &SrcSpan) -> Document<'a>
    where
        I: IntoIterator<Item = Document<'a>>,
    {
        let mut args = args.into_iter().peekable();
        if args.peek().is_none() {
            return "()".to_doc();
        }

        let args_doc = break_("", "")
            .append(join(args, break_(",", ", ")))
            .nest_if_broken(INDENT);

        // We get all remaining comments that come before the call's closing
        // parenthesis.
        // If there's any we add those before the closing parenthesis instead
        // of moving those out of the call.
        // Otherwise those would be moved out of the call.
        let comments = self.pop_comments(location.end);
        let closing_parens = match printed_comments(comments, false) {
            None => docvec!(break_(",", ""), ")"),
            Some(comment) => {
                docvec!(break_(",", "").nest(INDENT), comment, line(), ")").force_break()
            }
        };

        "(".to_doc().append(args_doc).append(closing_parens).group()
    }
}

fn init_and_last<T>(vec: &[T]) -> Option<(&[T], &T)> {
    match vec {
        [] => None,
        _ => match vec.split_at(vec.len() - 1) {
            (init, [last]) => Some((init, last)),
            _ => panic!("unreachable"),
        },
    }
}

impl<'a> Documentable<'a> for &'a ArgNames {
    fn to_doc(self) -> Document<'a> {
        match self {
            ArgNames::Named { name } | ArgNames::Discard { name } => name.to_doc(),
            ArgNames::LabelledDiscard { label, name } | ArgNames::NamedLabelled { label, name } => {
                docvec![label, " ", name]
            }
        }
    }
}

fn pub_(public: bool) -> Document<'static> {
    if public {
        "pub ".to_doc()
    } else {
        nil()
    }
}

impl<'a> Documentable<'a> for &'a UnqualifiedImport {
    fn to_doc(self) -> Document<'a> {
        self.name.as_str().to_doc().append(match &self.as_name {
            None => nil(),
            Some(s) => " as ".to_doc().append(s.as_str()),
        })
    }
}

impl<'a> Documentable<'a> for &'a BinOp {
    fn to_doc(self) -> Document<'a> {
        match self {
            BinOp::And => "&&",
            BinOp::Or => "||",
            BinOp::LtInt => "<",
            BinOp::LtEqInt => "<=",
            BinOp::LtFloat => "<.",
            BinOp::LtEqFloat => "<=.",
            BinOp::Eq => "==",
            BinOp::NotEq => "!=",
            BinOp::GtEqInt => ">=",
            BinOp::GtInt => ">",
            BinOp::GtEqFloat => ">=.",
            BinOp::GtFloat => ">.",
            BinOp::AddInt => "+",
            BinOp::AddFloat => "+.",
            BinOp::SubInt => "-",
            BinOp::SubFloat => "-.",
            BinOp::MultInt => "*",
            BinOp::MultFloat => "*.",
            BinOp::DivInt => "/",
            BinOp::DivFloat => "/.",
            BinOp::RemainderInt => "%",
            BinOp::Concatenate => "<>",
        }
        .to_doc()
    }
}

pub fn break_block(doc: Document<'_>) -> Document<'_> {
    "{".to_doc()
        .append(line().append(doc).nest(INDENT))
        .append(line())
        .append("}")
        .force_break()
}

pub fn wrap_block(doc: Document<'_>) -> Document<'_> {
    break_("{", "{ ")
        .append(doc)
        .nest(INDENT)
        .append(break_("", " "))
        .append("}")
}

pub fn wrap_args<'a, I>(args: I) -> Document<'a>
where
    I: IntoIterator<Item = Document<'a>>,
{
    let mut args = args.into_iter().peekable();
    if args.peek().is_none() {
        return "()".to_doc();
    }
    break_("(", "(")
        .append(join(args, break_(",", ", ")).next_break_fits(NextBreakFitsMode::Disabled))
        .nest(INDENT)
        .append(break_(",", ""))
        .append(")")
}

pub fn wrap_args_with_spread<'a, I>(args: I) -> Document<'a>
where
    I: IntoIterator<Item = Document<'a>>,
{
    let mut args = args.into_iter().peekable();
    if args.peek().is_none() {
        return "()".to_doc();
    }

    break_("(", "(")
        .append(join(args, break_(",", ", ")))
        .append(break_(",", ", "))
        .append("..")
        .nest(INDENT)
        .append(break_(",", ""))
        .append(")")
        .group()
}

fn bit_array<'a>(
    segments: impl IntoIterator<Item = Document<'a>>,
    is_simple: bool,
) -> Document<'a> {
    let comma = if is_simple {
        flex_break(",", ", ")
    } else {
        break_(",", ", ")
    };
    break_("<<", "<<")
        .append(join(segments, comma))
        .nest(INDENT)
        .append(break_(",", ""))
        .append(">>")
        .group()
}

fn printed_comments<'a, 'comments>(
    comments: impl IntoIterator<Item = Option<&'comments str>>,
    trailing_newline: bool,
) -> Option<Document<'a>> {
    let mut comments = comments.into_iter().peekable();
    let _ = comments.peek()?;

    let mut doc = Vec::new();
    while let Some(c) = comments.next() {
        let c = match c {
            Some(c) => c,
            None => continue,
        };
        doc.push("//".to_doc().append(Document::String(c.to_string())));
        match comments.peek() {
            // Next line is a comment
            Some(Some(_)) => doc.push(line()),
            // Next line is empty
            Some(None) => {
                let _ = comments.next();
                match comments.peek() {
                    Some(_) => doc.push(lines(2)),
                    None => {
                        if trailing_newline {
                            doc.push(lines(2));
                        }
                    }
                }
            }
            // We've reached the end, there are no more lines
            None => {
                if trailing_newline {
                    doc.push(line());
                }
            }
        }
    }
    let doc = concat(doc);
    if trailing_newline {
        Some(doc.force_break())
    } else {
        Some(doc)
    }
}

fn commented<'a, 'comments>(
    doc: Document<'a>,
    comments: impl IntoIterator<Item = Option<&'comments str>>,
) -> Document<'a> {
    match printed_comments(comments, true) {
        Some(comments) => comments.append(doc.group()),
        None => doc,
    }
}

fn bit_array_segment<Value, Type, ToDoc>(
    segment: &BitArraySegment<Value, Type>,
    mut to_doc: ToDoc,
) -> Document<'_>
where
    ToDoc: FnMut(&Value) -> Document<'_>,
{
    match segment {
        BitArraySegment { value, options, .. } if options.is_empty() => to_doc(value),

        BitArraySegment { value, options, .. } => to_doc(value).append(":").append(join(
            options.iter().map(|o| segment_option(o, |e| to_doc(e))),
            "-".to_doc(),
        )),
    }
}

fn segment_option<ToDoc, Value>(option: &BitArrayOption<Value>, mut to_doc: ToDoc) -> Document<'_>
where
    ToDoc: FnMut(&Value) -> Document<'_>,
{
    match option {
        BitArrayOption::Bytes { .. } => "bytes".to_doc(),
        BitArrayOption::Bits { .. } => "bits".to_doc(),
        BitArrayOption::Int { .. } => "int".to_doc(),
        BitArrayOption::Float { .. } => "float".to_doc(),
        BitArrayOption::Utf8 { .. } => "utf8".to_doc(),
        BitArrayOption::Utf16 { .. } => "utf16".to_doc(),
        BitArrayOption::Utf32 { .. } => "utf32".to_doc(),
        BitArrayOption::Utf8Codepoint { .. } => "utf8_codepoint".to_doc(),
        BitArrayOption::Utf16Codepoint { .. } => "utf16_codepoint".to_doc(),
        BitArrayOption::Utf32Codepoint { .. } => "utf32_codepoint".to_doc(),
        BitArrayOption::Signed { .. } => "signed".to_doc(),
        BitArrayOption::Unsigned { .. } => "unsigned".to_doc(),
        BitArrayOption::Big { .. } => "big".to_doc(),
        BitArrayOption::Little { .. } => "little".to_doc(),
        BitArrayOption::Native { .. } => "native".to_doc(),

        BitArrayOption::Size {
            value,
            short_form: false,
            ..
        } => "size"
            .to_doc()
            .append("(")
            .append(to_doc(value))
            .append(")"),

        BitArrayOption::Size {
            value,
            short_form: true,
            ..
        } => to_doc(value),

        BitArrayOption::Unit { value, .. } => "unit"
            .to_doc()
            .append("(")
            .append(Document::String(format!("{value}")))
            .append(")"),
    }
}

pub fn comments_before<'a>(
    comments: &'a [Comment<'a>],
    empty_lines: &'a [u32],
    limit: u32,
    retain_empty_lines: bool,
) -> (
    impl Iterator<Item = Option<&'a str>>,
    &'a [Comment<'a>],
    &'a [u32],
) {
    let end_comments = comments
        .iter()
        .position(|c| c.start > limit)
        .unwrap_or(comments.len());
    let end_empty_lines = empty_lines
        .iter()
        .position(|l| *l > limit)
        .unwrap_or(empty_lines.len());
    let popped_comments = comments
        .get(0..end_comments)
        .expect("0..end_comments is guaranteed to be in bounds")
        .iter()
        .map(|c| (c.start, Some(c.content)));
    let popped_empty_lines = if retain_empty_lines { empty_lines } else { &[] }
        .get(0..end_empty_lines)
        .unwrap_or(&[])
        .iter()
        .map(|i| (i, i))
        // compact consecutive empty lines into a single line
        .coalesce(|(a_start, a_end), (b_start, b_end)| {
            if *a_end + 1 == *b_start {
                Ok((a_start, b_end))
            } else {
                Err(((a_start, a_end), (b_start, b_end)))
            }
        })
        .map(|l| (*l.0, None));
    let popped = popped_comments
        .merge_by(popped_empty_lines, |(a, _), (b, _)| a < b)
        .skip_while(|(_, comment_or_line)| comment_or_line.is_none())
        .map(|(_, comment_or_line)| comment_or_line);
    (
        popped,
        comments.get(end_comments..).expect("in bounds"),
        empty_lines.get(end_empty_lines..).expect("in bounds"),
    )
}

fn is_breakable_expr(expr: &UntypedExpr) -> bool {
    matches!(
        expr,
        UntypedExpr::Fn { .. }
            | UntypedExpr::Block { .. }
            | UntypedExpr::Call { .. }
            | UntypedExpr::Case { .. }
            | UntypedExpr::List { .. }
            | UntypedExpr::Tuple { .. }
            | UntypedExpr::BitArray { .. }
    )
}

fn is_breakable_argument(expr: &UntypedExpr, arity: usize) -> bool {
    match expr {
        // A call is only breakable if it is the only argument
        UntypedExpr::Call { .. } => arity == 1,

        UntypedExpr::Fn { .. }
        | UntypedExpr::Block { .. }
        | UntypedExpr::Case { .. }
        | UntypedExpr::List { .. }
        | UntypedExpr::Tuple { .. }
        | UntypedExpr::BitArray { .. } => true,
        _ => false,
    }
}
