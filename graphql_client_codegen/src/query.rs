//! The responsibility of this module is to bind and validate a query
//! against a given schema.

mod fragments;
mod operations;

pub(crate) use fragments::FragmentRef;
pub(crate) use operations::OperationRef;

use crate::{
    constants::TYPENAME_FIELD,
    normalization::Normalization,
    schema::{
        resolve_field_type, EnumRef, InputRef, ScalarRef, Schema, StoredFieldId, StoredFieldType,
        TypeId, TypeRef, UnionRef,
    },
    type_qualifiers::GraphqlTypeQualifier,
};
use fragments::*;
use heck::CamelCase;
use operations::*;
use std::collections::{HashMap, HashSet};

/// This is a convenience struct that should stay private, it's an implementation detail for our `Ref` types.
#[derive(Copy, Clone)]
struct QueryWith<'a, T> {
    query: &'a Query,
    schema: &'a Schema,
    focus: T,
}

impl<'a, T> QueryWith<'a, T> {
    fn refocus<U>(&self, new_focus: U) -> QueryWith<'a, U> {
        QueryWith {
            query: self.query,
            schema: self.schema,
            focus: new_focus,
        }
    }
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub(crate) struct SelectionId(u32);
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct OperationId(u32);

impl OperationId {
    pub(crate) fn new(idx: usize) -> Self {
        OperationId(idx as u32)
    }
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub(crate) struct ResolvedFragmentId(u32);

#[derive(Debug, Clone, Copy)]
pub(crate) struct VariableId(u32);

impl VariableId {
    fn new(idx: usize) -> Self {
        VariableId(idx as u32)
    }
}

#[derive(Debug, Clone, Copy)]
enum SelectionParent {
    Field(SelectionId),
    InlineFragment(SelectionId),
    Fragment(ResolvedFragmentId),
    Operation(OperationId),
}

impl SelectionParent {
    fn add_to_selection_set(&self, q: &mut Query, selection_id: SelectionId) {
        match self {
            SelectionParent::Field(parent_selection_id)
            | SelectionParent::InlineFragment(parent_selection_id) => {
                let parent_selection = q
                    .selections
                    .get_mut(parent_selection_id.0 as usize)
                    .expect("get parent selection");

                match parent_selection {
                    Selection::Field(f) => f.selection_set.push(selection_id),
                    Selection::InlineFragment(inline) => inline.selection_set.push(selection_id),
                    other => unreachable!("impossible parent selection: {:?}", other),
                }
            }
            SelectionParent::Fragment(fragment_id) => {
                let fragment = q
                    .fragments
                    .get_mut(fragment_id.0 as usize)
                    .expect("get fragment");

                fragment.selection.push(selection_id);
            }
            SelectionParent::Operation(operation_id) => {
                let operation = q
                    .operations
                    .get_mut(operation_id.0 as usize)
                    .expect("get operation");

                operation.selection.push(selection_id);
            }
        }
    }
}

pub(crate) struct SelectionRef<'a>(QueryWith<'a, (SelectionId, &'a Selection)>);

impl<'a> SelectionRef<'a> {
    pub(crate) fn contains_fragment(&self, fragment_id: ResolvedFragmentId) -> bool {
        match self.selection() {
            Selection::FragmentSpread(id) => *id == fragment_id,
            _ => self
                .subselection()
                .any(|selection| selection.contains_fragment(fragment_id)),
        }
    }
    pub(crate) fn query(&self) -> &'a Query {
        self.0.query
    }

    pub(crate) fn schema(&self) -> &'a Schema {
        self.0.schema
    }

    pub(crate) fn selection(&self) -> &'a Selection {
        self.0.focus.1
    }

    fn id(&self) -> SelectionId {
        self.0.focus.0
    }

    pub(crate) fn subselection<'b>(&'b self) -> impl Iterator<Item = SelectionRef<'a>> + 'b {
        self.0
            .focus
            .1
            .subselection()
            .iter()
            .map(move |id| self.0.query.get_selection_ref(self.0.schema, *id))
    }

    pub(crate) fn subselection_ids(&self) -> &'a [SelectionId] {
        self.selection().subselection()
    }

    pub(crate) fn collect_used_types(&self, used_types: &mut UsedTypes) {
        let selection = self.selection();
        match selection {
            Selection::Field(field) => {
                let field_ref = self.0.schema.field(field.field_id);
                used_types.types.insert(field_ref.type_id());

                for item in self.subselection() {
                    item.collect_used_types(used_types);
                }
            }
            Selection::InlineFragment(inline_fragment) => {
                used_types.types.insert(inline_fragment.type_id);

                for item in self.subselection() {
                    item.collect_used_types(used_types);
                }
            }
            Selection::FragmentSpread(fragment_id) => {
                // This is necessary to avoid infinite recursion.
                if used_types.fragments.contains(fragment_id) {
                    return;
                }

                used_types.fragments.insert(*fragment_id);

                let fragment_ref = self.0.query.get_fragment_ref(self.0.schema, *fragment_id);

                for item in fragment_ref.selection_set() {
                    item.collect_used_types(used_types);
                }
            }
            Selection::Typename => (),
        }
    }

    pub(crate) fn full_path_prefix(&self) -> String {
        let mut path = match self.selection() {
            Selection::FragmentSpread(_) | Selection::InlineFragment(_) => Vec::new(),
            _ => vec![self.to_path_segment()],
        };

        let mut item = self.id();

        while let Some(parent) = self.0.query.selection_parent_idx.get(&item) {
            path.push(self.0.refocus(*parent).to_path_segment());

            match parent {
                SelectionParent::Field(id) | SelectionParent::InlineFragment(id) => {
                    item = *id;
                }
                _ => break,
            }
        }

        path.reverse();
        path.join("")
    }

    fn to_path_segment(&self) -> String {
        match self.selection() {
            Selection::Field(field) => field
                .alias
                .as_ref()
                .map(|alias| alias.to_camel_case())
                .unwrap_or_else(move || self.0.schema.field(field.field_id).name().to_camel_case()),
            Selection::InlineFragment(inline_fragment) => format!(
                "On{}",
                self.0
                    .schema
                    .type_ref(inline_fragment.type_id)
                    .name()
                    .to_camel_case()
            ),
            other => unreachable!("{:?} in to_path_segment", other),
        }
    }
}

impl<'a> QueryWith<'a, SelectionParent> {
    pub(crate) fn to_path_segment(&self) -> String {
        match self.focus {
            SelectionParent::Field(id) | SelectionParent::InlineFragment(id) => {
                SelectionRef(self.refocus((id, self.query.get_selection(id)))).to_path_segment()
            }
            SelectionParent::Operation(id) => OperationRef(self.refocus(id)).to_path_segment(),
            SelectionParent::Fragment(id) => self
                .query
                .get_fragment_ref(self.schema, id)
                .to_path_segment(),
        }
    }
}

#[derive(Debug)]
pub(crate) enum Selection {
    Field(SelectedField),
    InlineFragment(InlineFragment),
    FragmentSpread(ResolvedFragmentId),
    Typename,
}

impl Selection {
    pub(crate) fn subselection(&self) -> &[SelectionId] {
        match self {
            Selection::Field(field) => field.selection_set.as_slice(),
            Selection::InlineFragment(inline_fragment) => &inline_fragment.selection_set,
            _ => &[],
        }
    }
}

#[derive(Debug)]
pub(crate) struct InlineFragment {
    pub(crate) type_id: TypeId,
    // TODO: see if we can encode this at the top-level instead, with the selection being a parent.
    selection_set: Vec<SelectionId>,
}

#[derive(Debug)]
pub(crate) struct SelectedField {
    alias: Option<String>,
    field_id: StoredFieldId,
    selection_set: Vec<SelectionId>,
}

impl SelectedField {
    pub(crate) fn alias(&self) -> Option<&str> {
        self.alias.as_ref().map(String::as_str)
    }

    pub(crate) fn schema_field<'a>(&self, schema: &'a Schema) -> crate::schema::FieldRef<'a> {
        schema.field(self.field_id)
    }
}

pub(crate) fn resolve(
    schema: &Schema,
    query: &graphql_parser::query::Document,
) -> anyhow::Result<Query> {
    let mut resolved_query: Query = Default::default();

    // First, give ids to all fragments and operations.
    // TODO: refactor this into a "create_roots" function.
    for definition in &query.definitions {
        match definition {
            graphql_parser::query::Definition::Fragment(fragment) => {
                let graphql_parser::query::TypeCondition::On(on) = &fragment.type_condition;
                resolved_query.fragments.push(ResolvedFragment {
                    name: fragment.name.clone(),
                    on: schema.find_type(on).expect("TODO: proper error message"),
                    selection: Vec::new(),
                });
            }
            graphql_parser::query::Definition::Operation(
                graphql_parser::query::OperationDefinition::Mutation(m),
            ) => {
                let on = schema.mutation_type().ok_or_else(|| {
                    anyhow::anyhow!(
                        "Query contains a mutation operation, but the schema has no mutation type."
                    )
                })?;
                let resolved_operation: ResolvedOperation = ResolvedOperation {
                    object_id: on.id(),
                    name: m.name.as_ref().expect("mutation without name").to_owned(),
                    _operation_type: operations::OperationType::Mutation,
                    selection: Vec::with_capacity(m.selection_set.items.len()),
                };

                resolved_query.operations.push(resolved_operation);
            }
            graphql_parser::query::Definition::Operation(
                graphql_parser::query::OperationDefinition::Query(q),
            ) => {
                let on = schema.query_type();
                let resolved_operation: ResolvedOperation = ResolvedOperation {
                    name: q.name.as_ref().expect("query without name").to_owned(),
                    _operation_type: operations::OperationType::Query,
                    object_id: on.id(),
                    selection: Vec::with_capacity(q.selection_set.items.len()),
                };

                resolved_query.operations.push(resolved_operation);
            }
            graphql_parser::query::Definition::Operation(
                graphql_parser::query::OperationDefinition::Subscription(s),
            ) => {
                let on = schema.subscription_type().ok_or_else(|| {
                    anyhow::anyhow!(
                        "Query contains a subscription operation, but the schema has no subscription type."
                    )
                })?;

                if s.selection_set.items.len() != 1 {
                    anyhow::bail!("{}", crate::constants::MULTIPLE_SUBSCRIPTION_FIELDS_ERROR)
                }

                let resolved_operation: ResolvedOperation = ResolvedOperation {
                    name: s
                        .name
                        .as_ref()
                        .expect("subscription without name")
                        .to_owned(),
                    _operation_type: operations::OperationType::Subscription,
                    object_id: on.id(),
                    selection: Vec::with_capacity(s.selection_set.items.len()),
                };

                resolved_query.operations.push(resolved_operation);
            }
            graphql_parser::query::Definition::Operation(
                graphql_parser::query::OperationDefinition::SelectionSet(_),
            ) => anyhow::bail!("{}", crate::constants::SELECTION_SET_AT_ROOT),
        }
    }

    // Then resolve the selections.
    for definition in &query.definitions {
        match definition {
            graphql_parser::query::Definition::Fragment(fragment) => {
                resolve_fragment(&mut resolved_query, schema, fragment)?
            }
            graphql_parser::query::Definition::Operation(operation) => {
                resolve_operation(&mut resolved_query, schema, operation)?
            }
        }
    }

    Ok(resolved_query)
}

fn resolve_fragment(
    query: &mut Query,
    schema: &Schema,
    fragment_definition: &graphql_parser::query::FragmentDefinition,
) -> anyhow::Result<()> {
    let graphql_parser::query::TypeCondition::On(on) = &fragment_definition.type_condition;
    let on = schema.find_type(&on).unwrap();

    let (id, _) = query
        .find_fragment(&fragment_definition.name)
        .expect("TODO: fragment resolution");

    resolve_selection(
        query,
        schema,
        on,
        &fragment_definition.selection_set,
        SelectionParent::Fragment(id),
    )?;

    Ok(())
}

fn resolve_union_selection(
    query: &mut Query,
    union: UnionRef<'_>,
    selection_set: &graphql_parser::query::SelectionSet,
    parent: SelectionParent,
) -> anyhow::Result<()> {
    for item in selection_set.items.iter() {
        match item {
            graphql_parser::query::Selection::Field(field) => {
                if field.name == TYPENAME_FIELD {
                    let id = query.push_selection(Selection::Typename, parent);
                    parent.add_to_selection_set(query, id);
                } else {
                    anyhow::bail!("Invalid field selection on union field ({:?})", parent);
                }
            }
            graphql_parser::query::Selection::InlineFragment(inline_fragment) => {
                let selection_id =
                    resolve_inline_fragment(query, union.schema(), inline_fragment, parent)?;

                parent.add_to_selection_set(query, selection_id);
            }
            graphql_parser::query::Selection::FragmentSpread(fragment_spread) => {
                // TODO: this is very duplicated.
                let (fragment_id, _) = query
                    .find_fragment(&fragment_spread.fragment_name)
                    .expect("TODO: fragment resolution");

                let id = query.push_selection(Selection::FragmentSpread(fragment_id), parent);

                parent.add_to_selection_set(query, id);
            }
        }
    }

    Ok(())
}

fn resolve_object_selection<'a>(
    query: &mut Query,
    object: impl crate::schema::ObjectRefLike<'a>,
    selection_set: &graphql_parser::query::SelectionSet,
    parent: SelectionParent,
) -> anyhow::Result<()> {
    for item in selection_set.items.iter() {
        match item {
            graphql_parser::query::Selection::Field(field) => {
                if field.name == TYPENAME_FIELD {
                    let id = query.push_selection(Selection::Typename, parent);
                    parent.add_to_selection_set(query, id);
                    continue;
                }

                let field_ref = object.get_field_by_name(&field.name).ok_or_else(|| {
                    anyhow::anyhow!("No field named {} on {}", &field.name, object.name())
                })?;

                let id = query.push_selection(
                    Selection::Field(SelectedField {
                        alias: field.alias.clone(),
                        field_id: field_ref.field_id(),
                        selection_set: Vec::with_capacity(selection_set.items.len()),
                    }),
                    parent,
                );

                resolve_selection(
                    query,
                    object.schema(),
                    field_ref.type_id(),
                    &field.selection_set,
                    SelectionParent::Field(id),
                )?;

                parent.add_to_selection_set(query, id);
            }
            graphql_parser::query::Selection::InlineFragment(inline) => {
                let selection_id = resolve_inline_fragment(query, object.schema(), inline, parent)?;

                parent.add_to_selection_set(query, selection_id);
            }
            graphql_parser::query::Selection::FragmentSpread(fragment_spread) => {
                let (fragment_id, _) = query
                    .find_fragment(&fragment_spread.fragment_name)
                    .expect("TODO: fragment resolution");

                let id = query.push_selection(Selection::FragmentSpread(fragment_id), parent);

                parent.add_to_selection_set(query, id);
            }
        }
    }

    Ok(())
}

fn resolve_selection(
    ctx: &mut Query,
    schema: &Schema,
    on: TypeId,
    selection_set: &graphql_parser::query::SelectionSet,
    parent: SelectionParent,
) -> anyhow::Result<()> {
    match on {
        TypeId::Object(oid) => {
            let object = schema.object(oid);
            resolve_object_selection(ctx, object, selection_set, parent)?;
        }
        TypeId::Interface(interface_id) => {
            let interface = schema.interface(interface_id);
            resolve_object_selection(ctx, interface, selection_set, parent)?;
        }
        TypeId::Union(union_id) => {
            let union = schema.union(union_id);
            resolve_union_selection(ctx, union, selection_set, parent)?;
        }
        other => {
            anyhow::ensure!(
                selection_set.items.is_empty(),
                "Selection set on non-object, non-interface type. ({:?})",
                other
            );
        }
    };

    Ok(())
}

fn resolve_inline_fragment(
    query: &mut Query,
    schema: &Schema,
    inline_fragment: &graphql_parser::query::InlineFragment,
    parent: SelectionParent,
) -> anyhow::Result<SelectionId> {
    let graphql_parser::query::TypeCondition::On(on) = inline_fragment
        .type_condition
        .as_ref()
        .expect("missing type condition on inline fragment");
    let type_id = schema
        .find_type(on)
        .ok_or_else(|| anyhow::anyhow!("TODO: error message"))?;

    let id = query.push_selection(
        Selection::InlineFragment(InlineFragment {
            type_id,
            selection_set: Vec::with_capacity(inline_fragment.selection_set.items.len()),
        }),
        parent,
    );

    resolve_selection(
        query,
        schema,
        type_id,
        &inline_fragment.selection_set,
        SelectionParent::InlineFragment(id),
    )?;

    Ok(id)
}

fn resolve_operation(
    query: &mut Query,
    schema: &Schema,
    operation: &graphql_parser::query::OperationDefinition,
) -> anyhow::Result<()> {
    match operation {
        graphql_parser::query::OperationDefinition::Mutation(m) => {
            let on = schema.mutation_type().ok_or_else(|| {
                anyhow::anyhow!(
                    "Query contains a mutation operation, but the schema has no mutation type."
                )
            })?;
            let (id, _) = query.find_operation(m.name.as_ref().unwrap()).unwrap();

            resolve_variables(query, &m.variable_definitions, schema, id);
            resolve_object_selection(query, on, &m.selection_set, SelectionParent::Operation(id))?;
        }
        graphql_parser::query::OperationDefinition::Query(q) => {
            let on = schema.query_type();
            let (id, _) = query.find_operation(q.name.as_ref().unwrap()).unwrap();

            resolve_variables(query, &q.variable_definitions, schema, id);
            resolve_object_selection(query, on, &q.selection_set, SelectionParent::Operation(id))?;
        }
        graphql_parser::query::OperationDefinition::Subscription(s) => {
            let on = schema.subscription_type().ok_or_else(|| anyhow::anyhow!("Query contains a subscription operation, but the schema has no subscription type."))?;
            let (id, _) = query.find_operation(s.name.as_ref().unwrap()).unwrap();

            resolve_variables(query, &s.variable_definitions, schema, id);
            resolve_object_selection(query, on, &s.selection_set, SelectionParent::Operation(id))?;
        }
        graphql_parser::query::OperationDefinition::SelectionSet(_) => {
            unreachable!("unnamed queries are not supported")
        }
    }

    Ok(())
}

#[derive(Default)]
pub(crate) struct Query {
    fragments: Vec<ResolvedFragment>,
    operations: Vec<ResolvedOperation>,
    selection_parent_idx: HashMap<SelectionId, SelectionParent>,
    selections: Vec<Selection>,
    variables: Vec<ResolvedVariable>,
}

impl Query {
    fn push_selection(&mut self, node: Selection, parent: SelectionParent) -> SelectionId {
        let id = SelectionId(self.selections.len() as u32);
        self.selections.push(node);

        self.selection_parent_idx.insert(id, parent);

        id
    }

    pub(crate) fn get_selection_ref<'a>(
        &'a self,
        schema: &'a Schema,
        id: SelectionId,
    ) -> SelectionRef<'a> {
        let selection = self.get_selection(id);

        SelectionRef(self.with_schema(schema).refocus((id, selection)))
    }

    pub(crate) fn get_selection(&self, id: SelectionId) -> &Selection {
        self.selections
            .get(id.0 as usize)
            .expect("Query.get_selection")
    }

    fn get_fragment(&self, id: ResolvedFragmentId) -> &ResolvedFragment {
        self.fragments
            .get(id.0 as usize)
            .expect("Query.get_fragment")
    }

    fn get_variable(&self, id: VariableId) -> &ResolvedVariable {
        self.variables
            .get(id.0 as usize)
            .expect("Query.get_variable")
    }

    pub(crate) fn get_fragment_ref<'a>(
        &'a self,
        schema: &'a Schema,
        id: ResolvedFragmentId,
    ) -> FragmentRef<'a> {
        let fragment = self.get_fragment(id);

        FragmentRef(self.with_schema(schema).refocus((id, fragment)))
    }

    pub(crate) fn get_variable_ref<'a>(
        &'a self,
        schema: &'a Schema,
        id: VariableId,
    ) -> VariableRef<'a> {
        let variable = self.get_variable(id);

        VariableRef(self.with_schema(schema).refocus((id, variable)))
    }

    pub(crate) fn operations<'a>(
        &'a self,
        schema: &'a Schema,
    ) -> impl Iterator<Item = OperationRef<'a>> + 'a {
        (0..self.operations.len())
            .map(move |idx| OperationRef(self.with_schema(schema).refocus(OperationId(idx as u32))))
    }

    /// Selects the first operation matching `struct_name`. Returns `None` when the query document defines no operation, or when the selected operation does not match any defined operation.
    pub(crate) fn select_operation<'a>(
        &'a self,
        schema: &'a Schema,
        name: &str,
        normalization: &Normalization,
    ) -> Option<OperationRef<'a>> {
        self.operations(schema)
            .find(|op| normalization.operation(op.name()) == name)
    }

    fn find_fragment(&mut self, name: &str) -> Option<(ResolvedFragmentId, &mut ResolvedFragment)> {
        self.fragments
            .iter_mut()
            .enumerate()
            .find(|(_, frag)| frag.name == name)
            .map(|(id, f)| (ResolvedFragmentId(id as u32), f))
    }

    fn find_operation(&mut self, name: &str) -> Option<(OperationId, &mut ResolvedOperation)> {
        self.operations
            .iter_mut()
            .enumerate()
            .find(|(_, op)| op.name == name)
            .map(|(id, op)| (OperationId::new(id), op))
    }

    fn with_schema<'a>(&'a self, schema: &'a Schema) -> QueryWith<'a, ()> {
        QueryWith {
            focus: (),
            query: self,
            schema,
        }
    }
}

#[derive(Debug)]
struct ResolvedVariable {
    operation_id: OperationId,
    name: String,
    default: Option<graphql_parser::query::Value>,
    r#type: StoredFieldType,
}

#[derive(Copy, Clone)]
pub(crate) struct VariableRef<'a>(QueryWith<'a, (VariableId, &'a ResolvedVariable)>);

impl<'a> VariableRef<'a> {
    pub(crate) fn default(&self) -> Option<&graphql_parser::query::Value> {
        self.0.focus.1.default.as_ref()
    }

    pub(crate) fn name(&self) -> &'a str {
        &self.0.focus.1.name
    }

    pub(crate) fn variable_type(&self) -> TypeRef<'a> {
        self.0.schema.type_ref(self.0.focus.1.r#type.id)
    }

    pub(crate) fn type_name(&self) -> &'a str {
        self.0.schema.type_ref(self.0.focus.1.r#type.id).name()
    }

    pub(crate) fn type_qualifiers(&self) -> &[GraphqlTypeQualifier] {
        &self.0.focus.1.r#type.qualifiers
    }

    fn collect_used_types(&self, used_types: &mut UsedTypes) {
        match self.0.focus.1.r#type.id {
            TypeId::Input(input_id) => {
                used_types.types.insert(TypeId::Input(input_id));

                let input = self.0.schema.input(input_id);

                input.used_input_ids_recursive(used_types)
            }
            type_id @ TypeId::Scalar(_) | type_id @ TypeId::Enum(_) => {
                used_types.types.insert(type_id);
            }
            _ => (),
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct UsedTypes {
    pub(crate) types: HashSet<TypeId>,
    fragments: HashSet<ResolvedFragmentId>,
}

impl UsedTypes {
    pub(crate) fn inputs<'s, 'a: 's>(
        &'s self,
        schema: &'a Schema,
    ) -> impl Iterator<Item = InputRef<'a>> + 's {
        schema
            .inputs()
            .filter(move |input_ref| self.types.contains(&input_ref.type_id()))
    }

    pub(crate) fn scalars<'s, 'a: 's>(
        &'s self,
        schema: &'a Schema,
    ) -> impl Iterator<Item = ScalarRef<'a>> + 's {
        self.types
            .iter()
            .filter_map(TypeId::as_scalar_id)
            .map(move |scalar_id| schema.scalar(scalar_id))
            .filter(|scalar| !crate::schema::DEFAULT_SCALARS.contains(&scalar.name()))
    }

    pub(crate) fn enums<'a, 'schema: 'a>(
        &'a self,
        schema: &'schema Schema,
    ) -> impl Iterator<Item = EnumRef<'schema>> + 'a {
        self.types
            .iter()
            .filter_map(TypeId::as_enum_id)
            .map(move |enum_id| schema.r#enum(enum_id))
    }

    pub(crate) fn fragment_ids<'b>(&'b self) -> impl Iterator<Item = ResolvedFragmentId> + 'b {
        self.fragments.iter().map(|v| *v)
    }

    pub(crate) fn fragments_len(&self) -> usize {
        self.fragments.len()
    }
}

fn resolve_variables(
    query: &mut Query,
    variables: &[graphql_parser::query::VariableDefinition],
    schema: &Schema,
    operation_id: OperationId,
) {
    for var in variables {
        query.variables.push(ResolvedVariable {
            operation_id,
            name: var.name.clone(),
            default: var.default_value.clone(),
            r#type: resolve_field_type(schema, &var.var_type),
        });
    }
}
