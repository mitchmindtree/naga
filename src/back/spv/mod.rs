/*! Standard Portable Intermediate Representation (SPIR-V) backend
!*/

mod block;
mod helpers;
mod image;
mod index;
mod instructions;
mod layout;
mod recyclable;
mod selection;
mod writer;

pub use spirv::Capability;

use crate::{arena::Handle, back::BoundsCheckPolicies, proc::TypeResolution};

use spirv::Word;
use std::ops;
use thiserror::Error;

#[derive(Clone)]
struct PhysicalLayout {
    magic_number: Word,
    version: Word,
    generator: Word,
    bound: Word,
    instruction_schema: Word,
}

#[derive(Default)]
struct LogicalLayout {
    capabilities: Vec<Word>,
    extensions: Vec<Word>,
    ext_inst_imports: Vec<Word>,
    memory_model: Vec<Word>,
    entry_points: Vec<Word>,
    execution_modes: Vec<Word>,
    debugs: Vec<Word>,
    annotations: Vec<Word>,
    declarations: Vec<Word>,
    function_declarations: Vec<Word>,
    function_definitions: Vec<Word>,
}

struct Instruction {
    op: spirv::Op,
    wc: u32,
    type_id: Option<Word>,
    result_id: Option<Word>,
    operands: Vec<Word>,
}

const BITS_PER_BYTE: crate::Bytes = 8;

#[derive(Clone, Debug, Error)]
pub enum Error {
    #[error("The requested entry point couldn't be found")]
    EntryPointNotFound,
    #[error("target SPIRV-{0}.{1} is not supported")]
    UnsupportedVersion(u8, u8),
    #[error("using {0} requires at least one of the capabilities {1:?}, but none are available")]
    MissingCapabilities(&'static str, Vec<Capability>),
    #[error("unimplemented {0}")]
    FeatureNotImplemented(&'static str),
    #[error("module is not validated properly: {0}")]
    Validation(&'static str),
    #[error(transparent)]
    Proc(#[from] crate::proc::ProcError),
}

#[derive(Default)]
struct IdGenerator(Word);

impl IdGenerator {
    fn next(&mut self) -> Word {
        self.0 += 1;
        self.0
    }
}

/// A SPIR-V block to which we are still adding instructions.
///
/// A `Block` represents a SPIR-V block that does not yet have a termination
/// instruction like `OpBranch` or `OpReturn`.
///
/// The `OpLabel` that starts the block is implicit. It will be emitted based on
/// `label_id` when we write the block to a `LogicalLayout`.
///
/// To terminate a `Block`, pass the block and the termination instruction to
/// `Function::consume`. This takes ownership of the `Block` and transforms it
/// into a `TerminatedBlock`.
struct Block {
    label_id: Word,
    body: Vec<Instruction>,
}

/// A SPIR-V block that ends with a termination instruction.
struct TerminatedBlock {
    label_id: Word,
    body: Vec<Instruction>,
}

impl Block {
    fn new(label_id: Word) -> Self {
        Block {
            label_id,
            body: Vec::new(),
        }
    }
}

struct LocalVariable {
    id: Word,
    instruction: Instruction,
}

struct ResultMember {
    id: Word,
    type_id: Word,
    built_in: Option<crate::BuiltIn>,
}

struct EntryPointContext {
    argument_ids: Vec<Word>,
    results: Vec<ResultMember>,
}

#[derive(Default)]
struct Function {
    signature: Option<Instruction>,
    parameters: Vec<FunctionArgument>,
    variables: crate::FastHashMap<Handle<crate::LocalVariable>, LocalVariable>,
    blocks: Vec<TerminatedBlock>,
    entry_point_context: Option<EntryPointContext>,
}

impl Function {
    fn consume(&mut self, mut block: Block, termination: Instruction) {
        block.body.push(termination);
        self.blocks.push(TerminatedBlock {
            label_id: block.label_id,
            body: block.body,
        })
    }

    fn parameter_id(&self, index: u32) -> Word {
        match self.entry_point_context {
            Some(ref context) => context.argument_ids[index as usize],
            None => self.parameters[index as usize]
                .instruction
                .result_id
                .unwrap(),
        }
    }
}

/// Characteristics of a SPIR-V `OpTypeImage` type.
///
/// SPIR-V requires non-composite types to be unique, including images. Since we
/// use `LocalType` for this deduplication, it's essential that `LocalImageType`
/// be equal whenever the corresponding `OpTypeImage`s would be. To reduce the
/// likelihood of mistakes, we use fields that correspond exactly to the
/// operands of an `OpTypeImage` instruction, using the actual SPIR-V types
/// where practical.
#[derive(Debug, PartialEq, Hash, Eq, Copy, Clone)]
struct LocalImageType {
    sampled_type: crate::ScalarKind,
    dim: spirv::Dim,
    flags: ImageTypeFlags,
    image_format: spirv::ImageFormat,
}

bitflags::bitflags! {
    /// Flags corresponding to the boolean(-ish) parameters to OpTypeImage.
    pub struct ImageTypeFlags: u8 {
        const DEPTH = 0x1;
        const ARRAYED = 0x2;
        const MULTISAMPLED = 0x4;
        const SAMPLED = 0x8;
    }
}

impl LocalImageType {
    /// Construct a `LocalImageType` from the fields of a `TypeInner::Image`.
    fn from_inner(dim: crate::ImageDimension, arrayed: bool, class: crate::ImageClass) -> Self {
        let make_flags = |multi: bool, other: ImageTypeFlags| -> ImageTypeFlags {
            let mut flags = other;
            flags.set(ImageTypeFlags::ARRAYED, arrayed);
            flags.set(ImageTypeFlags::MULTISAMPLED, multi);
            flags
        };

        let dim = spirv::Dim::from(dim);

        match class {
            crate::ImageClass::Sampled { kind, multi } => LocalImageType {
                sampled_type: kind,
                dim,
                flags: make_flags(multi, ImageTypeFlags::SAMPLED),
                image_format: spirv::ImageFormat::Unknown,
            },
            crate::ImageClass::Depth { multi } => LocalImageType {
                sampled_type: crate::ScalarKind::Float,
                dim,
                flags: make_flags(multi, ImageTypeFlags::DEPTH | ImageTypeFlags::SAMPLED),
                image_format: spirv::ImageFormat::Unknown,
            },
            crate::ImageClass::Storage { format, access: _ } => LocalImageType {
                sampled_type: crate::ScalarKind::from(format),
                dim,
                flags: make_flags(false, ImageTypeFlags::empty()),
                image_format: format.into(),
            },
        }
    }
}

/// A SPIR-V type constructed during code generation.
///
/// This is the variant of [`LookupType`] used to represent types that might not
/// be available in the arena. Variants are present here for one of two reasons:
///
/// -   They represent types synthesized during code generation, as explained
///     in the documentation for [`LookupType`].
///
/// -   They represent types for which SPIR-V forbids duplicate `OpType...`
///     instructions, requiring deduplication.
///
/// This is not a complete copy of [`TypeInner`]: for example, SPIR-V generation
/// never synthesizes new struct types, so `LocalType` has nothing for that.
///
/// Each `LocalType` variant should be handled identically to its analogous
/// `TypeInner` variant. You can use the [`make_local`] function to help with
/// this, by converting everything possible to a `LocalType` before inspecting
/// it.
///
/// ## `Localtype` equality and SPIR-V `OpType` uniqueness
///
/// The definition of `Eq` on `LocalType` is carefully chosen to help us follow
/// certain SPIR-V rules. SPIR-V §2.8 requires some classes of `OpType...`
/// instructions to be unique; for example, you can't have two `OpTypeInt 32 1`
/// instructions in the same module. All 32-bit signed integers must use the
/// same type id.
///
/// All SPIR-V types that must be unique can be represented as a `LocalType`,
/// and two `LocalType`s are always `Eq` if SPIR-V would require them to use the
/// same `OpType...` instruction. This lets us avoid duplicates by recording the
/// ids of the type instructions we've already generated in a hash table,
/// [`Writer::lookup_type`], keyed by `LocalType`.
///
/// As another example, [`LocalImageType`], stored in the `LocalType::Image`
/// variant, is designed to help us deduplicate `OpTypeImage` instructions. See
/// its documentation for details.
///
/// `LocalType` also includes variants like `Pointer` that do not need to be
/// unique - but it is harmless to avoid the duplication.
///
/// As it always must, the `Hash` implementation respects the `Eq` relation.
///
/// [`TypeInner`]: crate::TypeInner
#[derive(Debug, PartialEq, Hash, Eq, Copy, Clone)]
enum LocalType {
    /// A scalar, vector, or pointer to one of those.
    Value {
        /// If `None`, this represents a scalar type. If `Some`, this represents
        /// a vector type of the given size.
        vector_size: Option<crate::VectorSize>,
        kind: crate::ScalarKind,
        width: crate::Bytes,
        pointer_class: Option<spirv::StorageClass>,
    },
    /// A matrix of floating-point values.
    Matrix {
        columns: crate::VectorSize,
        rows: crate::VectorSize,
        width: crate::Bytes,
    },
    Pointer {
        base: Handle<crate::Type>,
        class: spirv::StorageClass,
    },
    Image(LocalImageType),
    SampledImage {
        image_type_id: Word,
    },
    Sampler,
}

/// A type encountered during SPIR-V generation.
///
/// In the process of writing SPIR-V, we need to synthesize various types for
/// intermediate results and such: pointer types, vector/matrix component types,
/// or even booleans, which usually appear in SPIR-V code even when they're not
/// used by the module source.
///
/// However, we can't use `crate::Type` or `crate::TypeInner` for these, as the
/// type arena may not contain what we need (it only contains types used
/// directly by other parts of the IR), and the IR module is immutable, so we
/// can't add anything to it.
///
/// So for local use in the SPIR-V writer, we use this type, which holds either
/// a handle into the arena, or a [`LocalType`] containing something synthesized
/// locally.
///
/// This is very similar to the [`proc::TypeResolution`] enum, with `LocalType`
/// playing the role of `TypeInner`. However, `LocalType` also has other
/// properties needed for SPIR-V generation; see the description of
/// [`LocalType`] for details.
///
/// [`proc::TypeResolution`]: crate::proc::TypeResolution
#[derive(Debug, PartialEq, Hash, Eq, Copy, Clone)]
enum LookupType {
    Handle(Handle<crate::Type>),
    Local(LocalType),
}

impl From<LocalType> for LookupType {
    fn from(local: LocalType) -> Self {
        Self::Local(local)
    }
}

#[derive(Debug, PartialEq, Clone, Hash, Eq)]
struct LookupFunctionType {
    parameter_type_ids: Vec<Word>,
    return_type_id: Word,
}

fn make_local(inner: &crate::TypeInner) -> Option<LocalType> {
    Some(match *inner {
        crate::TypeInner::Scalar { kind, width } | crate::TypeInner::Atomic { kind, width } => {
            LocalType::Value {
                vector_size: None,
                kind,
                width,
                pointer_class: None,
            }
        }
        crate::TypeInner::Vector { size, kind, width } => LocalType::Value {
            vector_size: Some(size),
            kind,
            width,
            pointer_class: None,
        },
        crate::TypeInner::Matrix {
            columns,
            rows,
            width,
        } => LocalType::Matrix {
            columns,
            rows,
            width,
        },
        crate::TypeInner::Pointer { base, class } => LocalType::Pointer {
            base,
            class: helpers::map_storage_class(class),
        },
        crate::TypeInner::ValuePointer {
            size,
            kind,
            width,
            class,
        } => LocalType::Value {
            vector_size: size,
            kind,
            width,
            pointer_class: Some(helpers::map_storage_class(class)),
        },
        crate::TypeInner::Image {
            dim,
            arrayed,
            class,
        } => LocalType::Image(LocalImageType::from_inner(dim, arrayed, class)),
        crate::TypeInner::Sampler { comparison: _ } => LocalType::Sampler,
        _ => return None,
    })
}

#[derive(Debug)]
enum Dimension {
    Scalar,
    Vector,
    Matrix,
}

/// A map from evaluated [`Expression`](crate::Expression)s to their SPIR-V ids.
///
/// When we emit code to evaluate a given `Expression`, we record the
/// SPIR-V id of its value here, under its `Handle<Expression>` index.
///
/// A `CachedExpressions` value can be indexed by a `Handle<Expression>` value.
///
/// [emit]: index.html#expression-evaluation-time-and-scope
#[derive(Default)]
struct CachedExpressions {
    ids: Vec<Word>,
}
impl CachedExpressions {
    fn reset(&mut self, length: usize) {
        self.ids.clear();
        self.ids.resize(length, 0);
    }
}
impl ops::Index<Handle<crate::Expression>> for CachedExpressions {
    type Output = Word;
    fn index(&self, h: Handle<crate::Expression>) -> &Word {
        let id = &self.ids[h.index()];
        if *id == 0 {
            unreachable!("Expression {:?} is not cached!", h);
        }
        id
    }
}
impl ops::IndexMut<Handle<crate::Expression>> for CachedExpressions {
    fn index_mut(&mut self, h: Handle<crate::Expression>) -> &mut Word {
        let id = &mut self.ids[h.index()];
        if *id != 0 {
            unreachable!("Expression {:?} is already cached!", h);
        }
        id
    }
}
impl recyclable::Recyclable for CachedExpressions {
    fn recycle(self) -> Self {
        CachedExpressions {
            ids: self.ids.recycle(),
        }
    }
}

struct GlobalVariable {
    /// Actual ID of the variable.
    id: Word,
    /// For `StorageClass::Handle` variables, this ID is recorded in the function
    /// prelude block (and reset before every function) as `OpLoad` of the variable.
    /// It is then used for all the global ops, such as `OpImageSample`.
    handle_id: Word,
}

impl GlobalVariable {
    fn dummy() -> Self {
        Self {
            id: 0,
            handle_id: 0,
        }
    }

    fn new(id: Word) -> Self {
        Self { id, handle_id: 0 }
    }

    /// Prepare `self` for use within a single function.
    fn reset_for_function(&mut self) {
        self.handle_id = 0;
    }
}

struct FunctionArgument {
    /// Actual instruction of the argument.
    instruction: Instruction,
    handle_id: Word,
}

/// General information needed to emit SPIR-V for Naga statements.
struct BlockContext<'w> {
    /// The writer handling the module to which this code belongs.
    writer: &'w mut Writer,

    /// The [`Module`](crate::Module) for which we're generating code.
    ir_module: &'w crate::Module,

    /// The [`Function`](crate::Function) for which we're generating code.
    ir_function: &'w crate::Function,

    /// Information module validation produced about
    /// [`ir_function`](BlockContext::ir_function).
    fun_info: &'w crate::valid::FunctionInfo,

    /// The [`spv::Function`](Function) to which we are contributing SPIR-V instructions.
    function: &'w mut Function,

    /// SPIR-V ids for expressions we've evaluated.
    cached: CachedExpressions,

    /// The `Writer`'s temporary vector, for convenience.
    temp_list: Vec<Word>,
}

impl BlockContext<'_> {
    fn gen_id(&mut self) -> Word {
        self.writer.id_gen.next()
    }

    fn get_type_id(&mut self, lookup_type: LookupType) -> Word {
        self.writer.get_type_id(lookup_type)
    }

    fn get_expression_type_id(&mut self, tr: &TypeResolution) -> Word {
        self.writer.get_expression_type_id(tr)
    }

    fn get_index_constant(&mut self, index: Word) -> Word {
        self.writer
            .get_constant_scalar(crate::ScalarValue::Uint(index as _), 4)
    }

    fn get_scope_constant(&mut self, scope: Word) -> Word {
        self.writer
            .get_constant_scalar(crate::ScalarValue::Sint(scope as _), 4)
    }
}

#[derive(Clone, Copy, Default)]
struct LoopContext {
    continuing_id: Option<Word>,
    break_id: Option<Word>,
}

pub struct Writer {
    physical_layout: PhysicalLayout,
    logical_layout: LogicalLayout,
    id_gen: IdGenerator,

    /// The set of capabilities modules are permitted to use.
    ///
    /// This is initialized from `Options::capabilities`.
    capabilities_available: Option<crate::FastHashSet<Capability>>,

    /// The set of capabilities used by this module.
    ///
    /// If `capabilities_available` is `Some`, then this is always a subset of
    /// that.
    capabilities_used: crate::FastHashSet<Capability>,

    debugs: Vec<Instruction>,
    annotations: Vec<Instruction>,
    flags: WriterFlags,
    bounds_check_policies: BoundsCheckPolicies,
    void_type: Word,
    //TODO: convert most of these into vectors, addressable by handle indices
    lookup_type: crate::FastHashMap<LookupType, Word>,
    lookup_function: crate::FastHashMap<Handle<crate::Function>, Word>,
    lookup_function_type: crate::FastHashMap<LookupFunctionType, Word>,
    constant_ids: Vec<Word>,
    cached_constants: crate::FastHashMap<(crate::ScalarValue, crate::Bytes), Word>,
    global_variables: Vec<GlobalVariable>,

    // Cached expressions are only meaningful within a BlockContext, but we
    // retain the table here between functions to save heap allocations.
    saved_cached: CachedExpressions,

    gl450_ext_inst_id: Word,
    // Just a temporary list of SPIR-V ids
    temp_list: Vec<Word>,
}

bitflags::bitflags! {
    pub struct WriterFlags: u32 {
        /// Include debug labels for everything.
        const DEBUG = 0x1;
        /// Flip Y coordinate of `BuiltIn::Position` output.
        const ADJUST_COORDINATE_SPACE = 0x2;
        /// Emit `OpName` for input/output locations.
        /// Contrary to spec, some drivers treat it as semantic, not allowing
        /// any conflicts.
        const LABEL_VARYINGS = 0x4;
        /// Emit `PointSize` output builtin to vertex shaders, which is
        /// required for drawing with `PointList` topology.
        const FORCE_POINT_SIZE = 0x8;
    }
}

#[derive(Debug, Clone)]
pub struct Options {
    /// (Major, Minor) target version of the SPIR-V.
    pub lang_version: (u8, u8),

    /// Configuration flags for the writer.
    pub flags: WriterFlags,

    /// If given, the set of capabilities modules are allowed to use. Code that
    /// requires capabilities beyond these is rejected with an error.
    ///
    /// If this is `None`, all capabilities are permitted.
    pub capabilities: Option<crate::FastHashSet<Capability>>,

    /// How should generate code handle array, vector, matrix, or image texel
    /// indices that are out of range?
    pub bounds_check_policies: BoundsCheckPolicies,
}

impl Default for Options {
    fn default() -> Self {
        let mut flags = WriterFlags::ADJUST_COORDINATE_SPACE | WriterFlags::LABEL_VARYINGS;
        if cfg!(debug_assertions) {
            flags |= WriterFlags::DEBUG;
        }
        Options {
            lang_version: (1, 0),
            flags,
            capabilities: None,
            bounds_check_policies: super::BoundsCheckPolicies::default(),
        }
    }
}

// A subset of options that are meant to be changed per pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serialize", derive(serde::Serialize))]
#[cfg_attr(feature = "deserialize", derive(serde::Deserialize))]
pub struct PipelineOptions {
    /// The stage of the entry point
    pub shader_stage: crate::ShaderStage,
    /// The name of the entry point
    ///
    /// If no entry point that matches is found a error will be thrown while creating a new instance
    /// of [`Writer`](struct.Writer.html)
    pub entry_point: String,
}

pub fn write_vec(
    module: &crate::Module,
    info: &crate::valid::ModuleInfo,
    options: &Options,
    pipeline_options: Option<&PipelineOptions>,
) -> Result<Vec<u32>, Error> {
    let mut words = Vec::new();
    let mut w = Writer::new(options)?;
    w.write(module, info, pipeline_options, &mut words)?;
    Ok(words)
}
