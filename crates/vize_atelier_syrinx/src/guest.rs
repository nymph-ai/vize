//! DOM-less JavaScript guest assembly from Vize script-setup analysis.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use oxc_allocator::Allocator;
use oxc_ast::ast::Statement;
use oxc_ast_visit::{
    Visit,
    walk::{walk_arrow_function_expression, walk_for_of_statement, walk_function},
};
use oxc_parser::Parser;
use oxc_span::SourceType;
use oxc_syntax::scope::ScopeFlags;
use vize_atelier_sfc::{
    SfcDescriptor,
    compile_script::{extract_script_sections, typescript::transform_typescript_to_js},
    script::{ScriptCompileContext, gen_runtime_emits, transform_destructured_props},
    types::BindingType,
};

use crate::diagnostic::SyrinxDiagnostic;

#[derive(Debug, Clone)]
pub(crate) struct GuestHandler {
    pub id: u32,
    pub modifiers: Vec<String>,
    pub keys: Vec<String>,
    pub once: bool,
    pub capture: bool,
    pub passive: bool,
}

#[derive(Debug)]
pub(crate) struct GuestEmitOptions<'a> {
    pub component_id: u32,
    pub component_name: &'a str,
    pub input_names: &'a BTreeMap<u32, String>,
    pub required_capability_bits: u64,
    pub runtime_module: &'a str,
    pub install_body: &'a str,
    pub handlers: &'a [GuestHandler],
    pub filename: &'a str,
}

pub(crate) struct GuestEmitResult {
    pub code: String,
}

pub(crate) fn emit_guest(
    descriptor: &SfcDescriptor<'_>,
    options: &GuestEmitOptions<'_>,
) -> Result<GuestEmitResult, Vec<SyrinxDiagnostic>> {
    if descriptor.script.is_some() {
        return Err(vec![diagnostic(
            options.filename,
            "SYRINX_UNSUPPORTED_OPTIONS_SCRIPT",
            "A normal <script> block is outside the v2.2 SFC profile; move instance code to <script setup>.",
            descriptor
                .script
                .as_ref()
                .map(|block| block.loc.start)
                .unwrap_or(0),
            descriptor
                .script
                .as_ref()
                .map(|block| block.loc.end)
                .unwrap_or(0),
            descriptor.source.as_ref(),
        )]);
    }

    let content = descriptor
        .script_setup
        .as_ref()
        .map(|block| block.content.as_ref())
        .unwrap_or("");
    let source_is_ts = descriptor
        .script_setup
        .as_ref()
        .and_then(|block| block.lang.as_deref())
        .is_some_and(|lang| matches!(lang, "ts" | "tsx"));

    let mut context = ScriptCompileContext::new(content);
    context.analyze();
    let mut unsupported = Vec::new();
    for (present, call, code, message) in [
        (
            context.macros.define_slots.is_some(),
            context.macros.define_slots.as_ref(),
            "SYRINX_UNSUPPORTED_DEFINE_SLOTS",
            "defineSlots() requires the NYM-765 nested component/slot contract.",
        ),
        (
            !context.macros.define_models.is_empty(),
            context.macros.define_models.first(),
            "SYRINX_UNSUPPORTED_DEFINE_MODEL",
            "defineModel() is not in the v2.2 renderer-neutral SFC profile; use an explicit prop and emit.",
        ),
        (
            context.macros.define_options.is_some(),
            context.macros.define_options.as_ref(),
            "SYRINX_UNSUPPORTED_DEFINE_OPTIONS",
            "defineOptions() is not supported by the Syrinx guest backend.",
        ),
    ] {
        if present {
            let start = call.map(|call| call.start).unwrap_or(0);
            let end = call.map(|call| call.end).unwrap_or(start);
            let script_offset = descriptor
                .script_setup
                .as_ref()
                .map(|block| block.loc.start)
                .unwrap_or(0);
            unsupported.push(diagnostic(
                options.filename,
                code,
                message,
                script_offset + start,
                script_offset + end,
                descriptor.source.as_ref(),
            ));
        }
    }
    let source_type = match descriptor
        .script_setup
        .as_ref()
        .and_then(|block| block.lang.as_deref())
    {
        Some("ts") => SourceType::ts(),
        Some("tsx") => SourceType::tsx(),
        Some("jsx") => SourceType::jsx(),
        _ => SourceType::default(),
    }
    .with_module(true);
    if let Some((start, end)) = top_level_await_span(content, source_type) {
        let script_offset = descriptor
            .script_setup
            .as_ref()
            .map(|block| block.loc.start)
            .unwrap_or(0);
        unsupported.push(diagnostic(
            options.filename,
            "SYRINX_UNSUPPORTED_ASYNC_SETUP",
            "Top-level await would make guest setup nondeterministic; Syrinx setup must be synchronous.",
            script_offset + start,
            script_offset + end,
            descriptor.source.as_ref(),
        ));
    }
    if !unsupported.is_empty() {
        return Err(unsupported);
    }

    let Some((imports, setup_lines, declarations)) = extract_script_sections(content, source_is_ts)
    else {
        let offset = descriptor
            .script_setup
            .as_ref()
            .map(|block| block.loc.start)
            .unwrap_or(0);
        return Err(vec![diagnostic(
            options.filename,
            "SYRINX_SCRIPT_PARSE_FAILED",
            "Vize could not split <script setup> into module and instance sections.",
            offset,
            offset + content.len(),
            descriptor.source.as_ref(),
        )]);
    };

    let setup_source = setup_lines.join("\n");
    let transformed_setup = if let Some(destructure) = context.macros.props_destructure.as_ref() {
        transform_destructured_props(&setup_source, destructure).map_err(|error| {
            vec![diagnostic(
                options.filename,
                "SYRINX_PROPS_DESTRUCTURE_FAILED",
                error.message.as_str(),
                descriptor
                    .script_setup
                    .as_ref()
                    .map(|block| block.loc.start)
                    .unwrap_or(0),
                descriptor
                    .script_setup
                    .as_ref()
                    .map(|block| block.loc.end)
                    .unwrap_or(content.len()),
                descriptor.source.as_ref(),
            )]
        })?
    } else {
        setup_source.into()
    };

    let mut output = String::new();
    writeln!(
        output,
        "import {{ createCompiledSetup as __createCompiledSetup, displayValue as __displayValue, installBranch as __installBranch, installChild as __installChild, installConditionalSlot as __installConditionalSlot, installKeyedList as __installKeyedList, installSlotOutlet as __installSlotOutlet, invokeHandler as __invokeHandler, styleValue as __styleValue }} from {};",
        json(options.runtime_module)
    )
    .expect("String writes cannot fail");
    for import in imports {
        output.push_str(&rewrite_vue_import(import.as_str(), options.runtime_module));
        if !output.ends_with('\n') {
            output.push('\n');
        }
    }
    for declaration in declarations {
        output.push_str(declaration.as_str());
        output.push('\n');
    }

    output.push_str("\nconst __syrinxDefinition = {\n");
    writeln!(output, "  id: {},", options.component_id).unwrap();
    writeln!(output, "  name: {},", json(options.component_name)).unwrap();
    if let Some(props) = runtime_props(&context) {
        writeln!(output, "  props: {props},").unwrap();
    }
    if let Some(emits) = gen_runtime_emits(&context, &[]) {
        writeln!(output, "  emits: {},", emits).unwrap();
    }
    output.push_str("  inputNames: Object.freeze({");
    for (index, (id, name)) in options.input_names.iter().enumerate() {
        if index != 0 {
            output.push_str(", ");
        }
        write!(output, "{}: {}", id, json(name)).unwrap();
    }
    output.push_str("}),\n");
    writeln!(
        output,
        "  requiredCapabilityBits: {}n,",
        options.required_capability_bits
    )
    .unwrap();
    if !options.handlers.is_empty() {
        output.push_str("  eventHandlers: Object.freeze({\n");
        for handler in options.handlers {
            write!(
                output,
                "    {}: {{ callback: (__setup, __event, __context) => __setup.invoke({}, __event, __context)",
                handler.id, handler.id
            )
            .unwrap();
            if !handler.modifiers.is_empty() {
                write!(output, ", modifiers: {}", json_array(&handler.modifiers)).unwrap();
            }
            if !handler.keys.is_empty() {
                write!(output, ", keys: {}", json_array(&handler.keys)).unwrap();
            }
            if handler.once {
                output.push_str(", once: true");
            }
            if handler.capture {
                output.push_str(", capture: true");
            }
            if handler.passive {
                output.push_str(", passive: true");
            }
            output.push_str(" },\n");
        }
        output.push_str("  }),\n");
    }
    output.push_str("  setup(__props, __context) {\n");
    output.push_str("    const __emit = (event, ...args) => __context.emit(event, ...args);\n");
    output.push_str("    const __expose = value => __context.expose(value ?? {});\n");
    if let Some(call) = context.macros.define_emits.as_ref()
        && let Some(name) = call.binding_name.as_ref()
    {
        writeln!(output, "    const {name} = __emit;").unwrap();
    }
    if let Some(call) = context.macros.define_props.as_ref()
        && let Some(name) = call.binding_name.as_ref()
        && context.macros.props_destructure.is_none()
    {
        writeln!(output, "    const {name} = __props;").unwrap();
    }
    for line in transformed_setup.lines() {
        output.push_str("    ");
        output.push_str(line);
        output.push('\n');
    }
    if let Some(call) = context.macros.define_expose.as_ref() {
        writeln!(output, "    __expose({});", call.args.trim()).unwrap();
    }
    output.push_str("    return __createCompiledSetup((__scope, __compiled) => {\n");
    for line in options.install_body.lines() {
        output.push_str("      ");
        output.push_str(line);
        output.push('\n');
    }
    output.push_str("    });\n");
    output.push_str("  },\n");
    output.push_str("  render(__setup, __context) { __setup.install(__context); },\n");
    output.push_str("};\n\n");
    output.push_str("export default __syrinxDefinition;\n");
    output.push_str("export const definitions = Object.freeze([__syrinxDefinition]);\n");
    output.push_str("export const syrinxGuestAbi = Object.freeze({ version: 1, dom: false });\n");

    let code = if source_is_ts {
        transform_typescript_to_js(&output).to_string()
    } else {
        output
    };
    Ok(GuestEmitResult { code })
}

fn runtime_props(context: &ScriptCompileContext) -> Option<String> {
    let call = context.macros.define_props.as_ref()?;
    if !call.args.trim().is_empty() {
        return Some(call.args.trim().to_owned());
    }

    let mut names: Vec<_> = context
        .bindings
        .bindings
        .iter()
        .filter_map(|(name, binding)| {
            matches!(binding, BindingType::Props | BindingType::PropsAliased)
                .then_some(name.as_str())
        })
        .collect();
    names.sort_unstable();
    names.dedup();
    if names.is_empty() {
        return None;
    }
    let mut declaration = String::from("{");
    for (index, name) in names.iter().enumerate() {
        if index != 0 {
            declaration.push_str(", ");
        }
        write!(declaration, "{}: {{}}", json(name)).unwrap();
    }
    declaration.push('}');
    Some(declaration)
}

fn rewrite_vue_import(source: &str, runtime_module: &str) -> String {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, SourceType::ts().with_module(true)).parse();
    let Some(Statement::ImportDeclaration(declaration)) = parsed.program.body.first() else {
        return source.to_owned();
    };
    if declaration.source.value.as_str() != "vue" {
        return source.to_owned();
    }
    let mut rewritten = source.to_owned();
    rewritten.replace_range(
        declaration.source.span.start as usize..declaration.source.span.end as usize,
        &json(runtime_module),
    );
    rewritten
}

fn json(value: &str) -> String {
    serde_json::to_string(value).expect("strings are JSON serializable")
}

fn json_array(values: &[String]) -> String {
    serde_json::to_string(values).expect("strings are JSON serializable")
}

fn top_level_await_span(source: &str, source_type: SourceType) -> Option<(usize, usize)> {
    #[derive(Default)]
    struct TopLevelAwait {
        function_depth: usize,
        span: Option<(usize, usize)>,
    }

    impl<'a> Visit<'a> for TopLevelAwait {
        fn visit_function(&mut self, function: &oxc_ast::ast::Function<'a>, flags: ScopeFlags) {
            self.function_depth += 1;
            walk_function(self, function, flags);
            self.function_depth -= 1;
        }

        fn visit_arrow_function_expression(
            &mut self,
            arrow: &oxc_ast::ast::ArrowFunctionExpression<'a>,
        ) {
            self.function_depth += 1;
            walk_arrow_function_expression(self, arrow);
            self.function_depth -= 1;
        }

        fn visit_await_expression(&mut self, expression: &oxc_ast::ast::AwaitExpression<'a>) {
            if self.function_depth == 0 && self.span.is_none() {
                self.span = Some((expression.span.start as usize, expression.span.end as usize));
            }
        }

        fn visit_for_of_statement(&mut self, statement: &oxc_ast::ast::ForOfStatement<'a>) {
            if self.function_depth == 0 && statement.r#await && self.span.is_none() {
                self.span = Some((statement.span.start as usize, statement.span.end as usize));
            }
            walk_for_of_statement(self, statement);
        }
    }

    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked || !parsed.diagnostics.is_empty() {
        return None;
    }
    let mut visitor = TopLevelAwait::default();
    visitor.visit_program(&parsed.program);
    visitor.span
}

fn diagnostic(
    filename: &str,
    code: &str,
    message: &str,
    start: usize,
    end: usize,
    source: &str,
) -> SyrinxDiagnostic {
    let (start_line, start_column) = line_column(source, start);
    let (end_line, end_column) = line_column(source, end);
    SyrinxDiagnostic {
        code: code.to_owned(),
        message: message.to_owned(),
        source: filename.to_owned(),
        start_byte: start as u32,
        end_byte: end as u32,
        start_line,
        start_column,
        end_line,
        end_column,
    }
}

fn line_column(source: &str, offset: usize) -> (u32, u32) {
    let offset = offset.min(source.len());
    let prefix = &source[..offset];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32 + 1;
    let column = prefix
        .rfind('\n')
        .map_or(offset, |newline| offset.saturating_sub(newline + 1)) as u32
        + 1;
    (line, column)
}
