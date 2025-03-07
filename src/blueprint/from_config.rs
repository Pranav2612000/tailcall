#![allow(clippy::too_many_arguments)]

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use async_graphql::parser::types::ConstDirective;
#[allow(unused_imports)]
use async_graphql::InputType;
use async_graphql_value::ConstValue;
use hyper::header::{HeaderName, HeaderValue};
use hyper::HeaderMap;
use regex::Regex;

use super::UnionTypeDefinition;
use crate::blueprint::Type::ListType;
use crate::blueprint::*;
use crate::config::group_by::GroupBy;
use crate::config::{Arg, Batch, Config, Field, InlineType};
use crate::directive::DirectiveCodec;
use crate::endpoint::Endpoint;
use crate::http::Method;
use crate::json::JsonSchema;
use crate::lambda::Expression::Literal;
use crate::lambda::{Expression, Lambda, Operation};
use crate::request_template::RequestTemplate;
use crate::valid::{Valid, ValidationError};
use crate::{blueprint, config};

pub fn config_blueprint(config: &Config) -> Valid<Blueprint, String> {
  let output_types = config.output_types();
  let input_types = config.input_types();
  let schema = to_schema(config);
  let definitions = to_definitions(config, output_types, input_types);
  let server = Server::try_from(config.server.clone()).into();
  let upstream = to_upstream(config.upstream.clone());

  schema
    .zip(definitions)
    .zip(server)
    .zip(upstream)
    .map(|(((schema, definitions), server), upstream)| Blueprint { schema, definitions, server, upstream })
    .map(apply_batching)
    .map(super::compress::compress)
}

fn to_upstream(upstream: config::Upstream) -> Valid<config::Upstream, String> {
  if let Some(ref base_url) = upstream.base_url {
    Valid::from(reqwest::Url::parse(base_url).map_err(|e| ValidationError::new(e.to_string()))).map_to(upstream)
  } else {
    Valid::succeed(upstream)
  }
}

pub fn apply_batching(mut blueprint: Blueprint) -> Blueprint {
  for def in blueprint.definitions.iter() {
    if let Definition::ObjectTypeDefinition(object_type_definition) = def {
      for field in object_type_definition.fields.iter() {
        if let Some(Expression::Unsafe(Operation::Endpoint(_request_template, Some(_), _dl))) = field.resolver.clone() {
          blueprint.upstream.batch = blueprint.upstream.batch.or(Some(Batch::default()));
          return blueprint;
        }
      }
    }
  }
  blueprint
}

fn to_directive(const_directive: ConstDirective) -> Valid<Directive, String> {
  const_directive
    .arguments
    .into_iter()
    .map(|(k, v)| {
      let value = v.node.into_json();
      if let Ok(value) = value {
        return Ok((k.node.to_string(), value));
      }
      Err(value.unwrap_err())
    })
    .collect::<Result<HashMap<String, serde_json::Value>, _>>()
    .map_err(|e| ValidationError::new(e.to_string()))
    .map(|arguments| Directive { name: const_directive.name.node.clone().to_string(), arguments, index: 0 })
    .into()
}

fn to_schema(config: &Config) -> Valid<SchemaDefinition, String> {
  validate_query(config)
    .and(validate_mutation(config))
    .and(Valid::from_option(
      config.graphql.schema.query.as_ref(),
      "Query root is missing".to_owned(),
    ))
    .zip(to_directive(config.server.to_directive("server".to_string())))
    .map(|(query_type_name, directive)| SchemaDefinition {
      query: query_type_name.to_owned(),
      mutation: config.graphql.schema.mutation.clone(),
      directives: vec![directive],
    })
}

fn to_definitions<'a>(
  config: &Config,
  output_types: HashSet<&'a String>,
  input_types: HashSet<&'a String>,
) -> Valid<Vec<Definition>, String> {
  Valid::from_iter(config.graphql.types.iter(), |(name, type_)| {
    let dbl_usage = input_types.contains(name) && output_types.contains(name);
    if let Some(variants) = &type_.variants {
      if !variants.is_empty() {
        to_enum_type_definition(name, type_, config, variants.clone()).trace(name)
      } else {
        Valid::fail("No variants found for enum".to_string())
      }
    } else if type_.scalar {
      to_scalar_type_definition(name).trace(name)
    } else if dbl_usage {
      Valid::fail("type is used in input and output".to_string()).trace(name)
    } else {
      to_object_type_definition(name, type_, config)
        .trace(name)
        .and_then(|definition| match definition.clone() {
          Definition::ObjectTypeDefinition(object_type_definition) => {
            if config.input_types().contains(name) {
              to_input_object_type_definition(object_type_definition).trace(name)
            } else if type_.interface {
              to_interface_type_definition(object_type_definition).trace(name)
            } else {
              Valid::succeed(definition)
            }
          }
          _ => Valid::succeed(definition),
        })
    }
  })
  .map(|mut types| {
    types.extend(
      config
        .graphql
        .unions
        .iter()
        .map(to_union_type_definition)
        .map(Definition::UnionTypeDefinition),
    );
    types
  })
}
fn to_scalar_type_definition(name: &str) -> Valid<Definition, String> {
  Valid::succeed(Definition::ScalarTypeDefinition(ScalarTypeDefinition {
    name: name.to_string(),
    directive: Vec::new(),
    description: None,
  }))
}
fn to_union_type_definition((name, u): (&String, &config::Union)) -> UnionTypeDefinition {
  UnionTypeDefinition {
    name: name.to_owned(),
    description: u.doc.clone(),
    directives: Vec::new(),
    types: u.types.clone(),
  }
}
fn to_enum_type_definition(
  name: &str,
  type_: &config::Type,
  _config: &Config,
  variants: BTreeSet<String>,
) -> Valid<Definition, String> {
  let enum_type_definition = Definition::EnumTypeDefinition(EnumTypeDefinition {
    name: name.to_string(),
    directives: Vec::new(),
    description: type_.doc.clone(),
    enum_values: variants
      .iter()
      .map(|variant| EnumValueDefinition { description: None, name: variant.clone(), directives: Vec::new() })
      .collect(),
  });
  Valid::succeed(enum_type_definition)
}
fn to_object_type_definition(name: &str, type_of: &config::Type, config: &Config) -> Valid<Definition, String> {
  to_fields(type_of, config).map(|fields| {
    Definition::ObjectTypeDefinition(ObjectTypeDefinition {
      name: name.to_string(),
      description: type_of.doc.clone(),
      fields,
      implements: type_of.implements.clone(),
    })
  })
}
fn to_input_object_type_definition(definition: ObjectTypeDefinition) -> Valid<Definition, String> {
  Valid::succeed(Definition::InputObjectTypeDefinition(InputObjectTypeDefinition {
    name: definition.name,
    fields: definition
      .fields
      .iter()
      .map(|field| InputFieldDefinition {
        name: field.name.clone(),
        description: field.description.clone(),
        default_value: None,
        of_type: field.of_type.clone(),
      })
      .collect(),
    description: definition.description,
  }))
}
fn to_interface_type_definition(definition: ObjectTypeDefinition) -> Valid<Definition, String> {
  Valid::succeed(Definition::InterfaceTypeDefinition(InterfaceTypeDefinition {
    name: definition.name,
    fields: definition.fields,
    description: definition.description,
  }))
}
fn to_fields(type_of: &config::Type, config: &Config) -> Valid<Vec<blueprint::FieldDefinition>, String> {
  Valid::from_iter(type_of.fields.iter(), |(name, field)| {
    validate_field_type_exist(config, field)
      .and(to_field(type_of, config, name, field))
      .trace(name)
  })
  .map(|fields| fields.into_iter().flatten().collect())
}

fn get_value_type(type_of: &config::Type, value: &str) -> Option<Type> {
  if let Some(field) = type_of.fields.get(value) {
    return Some(to_type(
      &field.type_of,
      field.list,
      field.required,
      field.list_type_required,
    ));
  }

  None
}

fn validate_mustache_parts(
  type_of: &config::Type,
  config: &Config,
  is_query: bool,
  parts: &[String],
  args: &[InputFieldDefinition],
) -> Valid<(), String> {
  if parts.len() < 2 {
    return Valid::fail("too few parts in template".to_string());
  }

  let head = parts[0].as_str();
  let tail = parts[1].as_str();

  match head {
    "value" => {
      if let Some(val_type) = get_value_type(type_of, tail) {
        if !is_scalar(val_type.name()) {
          return Valid::fail(format!("value '{tail}' is not of a scalar type"));
        }

        // Queries can use optional values
        if !is_query && val_type.is_nullable() {
          return Valid::fail(format!("value '{tail}' is a nullable type"));
        }
      } else {
        return Valid::fail(format!("no value '{tail}' found"));
      }
    }
    "args" => {
      // XXX this is a linear search but it's cost is less than that of
      // constructing a HashMap since we'd have 3-4 arguments at max in
      // most cases
      if let Some(arg) = args.iter().find(|arg| arg.name == tail) {
        if let Type::ListType { .. } = arg.of_type {
          return Valid::fail(format!("can't use list type '{tail}' here"));
        }

        // we can use non-scalar types in args

        if !is_query && arg.default_value.is_none() && arg.of_type.is_nullable() {
          return Valid::fail(format!("argument '{tail}' is a nullable type"));
        }
      } else {
        return Valid::fail(format!("no argument '{tail}' found"));
      }
    }
    "vars" => {
      if config.server.vars.get(tail).is_none() {
        return Valid::fail(format!("var '{tail}' is not set in the server config"));
      }
    }
    "headers" => {
      // "headers" refers to the header values known at runtime, which we can't
      // validate here
    }
    _ => {
      return Valid::fail(format!("unknown template directive '{head}'"));
    }
  }

  Valid::succeed(())
}

fn validate_field(type_of: &config::Type, config: &Config, field: &FieldDefinition) -> Valid<(), String> {
  // XXX we could use `Mustache`'s `render` method with a mock
  // struct implementing the `PathString` trait encapsulating `validation_map`
  // but `render` simply falls back to the default value for a given
  // type if it doesn't exist, so we wouldn't be able to get enough
  // context from that method alone
  // So we must duplicate some of that logic here :(
  if let Some(Expression::Unsafe(Operation::Endpoint(req_template, _, _))) = &field.resolver {
    Valid::from_iter(req_template.root_url.expression_segments(), |parts| {
      validate_mustache_parts(type_of, config, false, parts, &field.args).trace("path")
    })
    .and(Valid::from_iter(req_template.query.clone(), |query| {
      let (_, mustache) = query;

      Valid::from_iter(mustache.expression_segments(), |parts| {
        validate_mustache_parts(type_of, config, true, parts, &field.args).trace("query")
      })
    }))
    .unit()
  } else {
    Valid::succeed(())
  }
}

fn to_field(
  type_of: &config::Type,
  config: &Config,
  name: &str,
  field: &Field,
) -> Valid<Option<blueprint::FieldDefinition>, String> {
  let directives = field.resolvable_directives();
  if directives.len() > 1 {
    return Valid::fail(format!("Multiple resolvers detected [{}]", directives.join(", ")));
  }

  let field_type = &field.type_of;
  to_args(field).and_then(|args| {
    let field_definition = FieldDefinition {
      name: name.to_owned(),
      description: field.doc.clone(),
      args,
      of_type: to_type(field_type, field.list, field.required, field.list_type_required),
      directives: Vec::new(),
      resolver: None,
    };

    update_http(field, field_definition, type_of, config)
      .trace("@http")
      .map(|field_definition| update_unsafe(field.clone(), field_definition))
      .and_then(|field_definition| update_const_field(field, field_definition, config).trace("@const"))
      .and_then(|field_definition| update_inline_field(type_of, field, field_definition, config).trace("@inline"))
      .and_then(|field_definition| update_modify(field, field_definition, type_of, config).trace("@modify"))
  })
}

fn to_type(name: &str, list: bool, non_null: bool, list_type_required: bool) -> Type {
  if list {
    Type::ListType {
      of_type: Box::new(Type::NamedType { name: name.to_string(), non_null: list_type_required }),
      non_null,
    }
  } else {
    Type::NamedType { name: name.to_string(), non_null }
  }
}

fn validate_query(config: &Config) -> Valid<(), String> {
  Valid::from_option(config.graphql.schema.query.clone(), "Query root is missing".to_owned())
    .and_then(|ref query_type_name| {
      let Some(query) = config.find_type(query_type_name) else {
        return Valid::fail("Query type is not defined".to_owned()).trace(query_type_name);
      };

      Valid::from_iter(query.fields.iter(), validate_field_has_resolver).trace(query_type_name)
    })
    .unit()
}

fn validate_mutation(config: &Config) -> Valid<(), String> {
  let mutation_type_name = config.graphql.schema.mutation.as_ref();

  if let Some(mutation_type_name) = mutation_type_name {
    let Some(mutation) = config.find_type(mutation_type_name) else {
      return Valid::fail("Mutation type is not defined".to_owned()).trace(mutation_type_name);
    };

    Valid::from_iter(mutation.fields.iter(), validate_field_has_resolver)
      .trace(mutation_type_name)
      .unit()
  } else {
    Valid::succeed(())
  }
}

fn validate_field_has_resolver((name, field): (&String, &Field)) -> Valid<(), String> {
  Valid::<(), String>::fail("No resolver has been found in the schema".to_owned())
    .when(|| !field.has_resolver())
    .trace(name)
}

fn validate_field_type_exist(config: &Config, field: &Field) -> Valid<(), String> {
  let field_type = &field.type_of;
  if !is_scalar(field_type) && !config.contains(field_type) {
    Valid::fail(format!("Undeclared type '{field_type}' was found"))
  } else {
    Valid::succeed(())
  }
}

fn update_unsafe(field: config::Field, mut b_field: FieldDefinition) -> FieldDefinition {
  if let Some(op) = field.unsafe_operation {
    b_field = b_field.resolver_or_default(Lambda::context().to_unsafe_js(op.script.clone()), |r| {
      r.to_unsafe_js(op.script.clone())
    });
  }
  b_field
}

fn update_http(
  field: &config::Field,
  b_field: FieldDefinition,
  type_of: &config::Type,
  config: &Config,
) -> Valid<FieldDefinition, String> {
  match field.http.as_ref() {
    Some(http) => match http
      .base_url
      .as_ref()
      .map_or_else(|| config.upstream.base_url.as_ref(), Some)
    {
      Some(base_url) => {
        let mut base_url = base_url.clone();
        if base_url.ends_with('/') {
          base_url.pop();
        }
        base_url.push_str(http.path.clone().as_str());
        let query = http.query.clone().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let output_schema = to_json_schema_for_field(field, config);
        let input_schema = to_json_schema_for_args(&field.args, config);

        Valid::<(), String>::fail("GroupBy is only supported for GET requests".to_string())
          .when(|| !http.group_by.is_empty() && http.method != Method::GET)
          .and(Valid::from_iter(http.headers.iter(), |(k, v)| {
            let name =
              Valid::from(HeaderName::from_bytes(k.as_bytes()).map_err(|e| ValidationError::new(e.to_string())));

            let value = Valid::from(HeaderValue::from_str(v.as_str()).map_err(|e| ValidationError::new(e.to_string())));

            name.zip(value).map(|(name, value)| (name, value))
          }))
          .map(HeaderMap::from_iter)
          .and_then(|header_map| {
            RequestTemplate::try_from(
              Endpoint::new(base_url.to_string())
                .method(http.method.clone())
                .query(query)
                .output(output_schema)
                .input(input_schema)
                .body(http.body.clone())
                .headers(header_map),
            )
            .map_err(|e| ValidationError::new(e.to_string()))
            .into()
          })
          .map(|req_template| {
            if !http.group_by.is_empty() && http.method == Method::GET {
              b_field.resolver(Some(Expression::Unsafe(Operation::Endpoint(
                req_template,
                Some(GroupBy::new(http.group_by.clone())),
                None,
              ))))
            } else {
              b_field.resolver(Some(Lambda::from_request_template(req_template).expression))
            }
          })
          .and_then(|b_field| validate_field(type_of, config, &b_field).map_to(b_field))
      }
      None => Valid::fail("No base URL defined".to_string()),
    },
    None => Valid::succeed(b_field),
  }
}
fn update_modify(
  field: &config::Field,
  mut b_field: FieldDefinition,
  type_: &config::Type,
  config: &Config,
) -> Valid<Option<FieldDefinition>, String> {
  match field.modify.as_ref() {
    Some(modify) => {
      if modify.omit {
        Valid::succeed(None)
      } else if let Some(new_name) = &modify.name {
        for name in type_.implements.iter() {
          let interface = config.find_type(name);
          if let Some(interface) = interface {
            if interface.fields.iter().any(|(name, _)| name == new_name) {
              return Valid::fail("Field is already implemented from interface".to_string());
            }
          }
        }

        let lambda = Lambda::context_field(b_field.name.clone());
        b_field = b_field.resolver_or_default(lambda, |r| r);
        b_field = b_field.name(new_name.clone());
        Valid::succeed(Some(b_field))
      } else {
        Valid::succeed(Some(b_field))
      }
    }
    None => Valid::succeed(Some(b_field)),
  }
}
fn update_const_field(
  field: &config::Field,
  mut b_field: FieldDefinition,
  config: &Config,
) -> Valid<FieldDefinition, String> {
  match field.const_field.as_ref() {
    Some(const_field) => {
      let data = const_field.data.to_owned();
      match ConstValue::from_json(data.to_owned()) {
        Ok(gql_value) => match to_json_schema_for_field(field, config).validate(&gql_value).to_result() {
          Ok(_) => {
            b_field.resolver = Some(Literal(data));
            Valid::succeed(b_field)
          }
          Err(err) => Valid::from_validation_err(err.transform(|a| a.to_owned())),
        },
        Err(e) => Valid::fail(format!("invalid JSON: {}", e)),
      }
    }
    None => Valid::succeed(b_field),
  }
}
fn is_scalar(type_name: &str) -> bool {
  ["String", "Int", "Float", "Boolean", "ID", "JSON"].contains(&type_name)
}
// Helper function to recursively process the path and return the corresponding type
fn process_path(
  path: &[String],
  field: &config::Field,
  type_info: &config::Type,
  is_required: bool,
  config: &Config,
  invalid_path_handler: &dyn Fn(&str, &[String]) -> Valid<Type, String>,
) -> Valid<Type, String> {
  if let Some((field_name, remaining_path)) = path.split_first() {
    if field_name.parse::<usize>().is_ok() {
      let mut modified_field = field.clone();
      modified_field.list = false;
      return process_path(
        remaining_path,
        &modified_field,
        type_info,
        false,
        config,
        invalid_path_handler,
      );
    }
    let target_type_info = type_info
      .fields
      .get(field_name)
      .map(|_| type_info)
      .or_else(|| config.find_type(&field.type_of));

    if let Some(type_info) = target_type_info {
      return process_field_within_type(
        field,
        field_name,
        remaining_path,
        type_info,
        is_required,
        config,
        invalid_path_handler,
      );
    }
    return invalid_path_handler(field_name, path);
  }

  Valid::succeed(to_type(
    &field.type_of,
    field.list,
    is_required,
    field.list_type_required,
  ))
}

fn process_field_within_type(
  field: &config::Field,
  field_name: &str,
  remaining_path: &[String],
  type_info: &config::Type,
  is_required: bool,
  config: &Config,
  invalid_path_handler: &dyn Fn(&str, &[String]) -> Valid<Type, String>,
) -> Valid<Type, String> {
  if let Some(next_field) = type_info.fields.get(field_name) {
    if next_field.has_resolver() {
      return Valid::<Type, String>::fail(format!(
        "Inline can't be done because of {} resolver at [{}.{}]",
        {
          let next_dir_http = next_field.http.as_ref().map(|_| "http");
          let next_dir_const = next_field.const_field.as_ref().map(|_| "const");
          next_dir_http.or(next_dir_const).unwrap_or("unsafe")
        },
        field.type_of,
        field_name
      ))
      .and(process_path(
        remaining_path,
        next_field,
        type_info,
        is_required,
        config,
        invalid_path_handler,
      ));
    }

    let next_is_required = is_required && next_field.required;
    if is_scalar(&next_field.type_of) {
      return process_path(
        remaining_path,
        next_field,
        type_info,
        next_is_required,
        config,
        invalid_path_handler,
      );
    }

    if let Some(next_type_info) = config.find_type(&next_field.type_of) {
      return process_path(
        remaining_path,
        next_field,
        next_type_info,
        next_is_required,
        config,
        invalid_path_handler,
      )
      .and_then(|of_type| {
        if next_field.list {
          Valid::succeed(ListType { of_type: Box::new(of_type), non_null: is_required })
        } else {
          Valid::succeed(of_type)
        }
      });
    }
  } else if let Some((head, tail)) = remaining_path.split_first() {
    if let Some(field) = type_info.fields.get(head) {
      return process_path(tail, field, type_info, is_required, config, invalid_path_handler);
    }
  }

  invalid_path_handler(field_name, remaining_path)
}

// Main function to update an inline field
fn update_inline_field(
  type_info: &config::Type,
  field: &config::Field,
  base_field: FieldDefinition,
  config: &Config,
) -> Valid<FieldDefinition, String> {
  let inlined_path = field.inline.as_ref().map(|x| x.path.clone()).unwrap_or_default();
  let handle_invalid_path = |_field_name: &str, _inlined_path: &[String]| -> Valid<Type, String> {
    Valid::fail("Inline can't be done because provided path doesn't exist".to_string())
  };
  let has_index = inlined_path.iter().any(|s| {
    let re = Regex::new(r"^\d+$").unwrap();
    re.is_match(s)
  });
  if let Some(InlineType { path }) = field.clone().inline {
    return process_path(&inlined_path, field, type_info, false, config, &handle_invalid_path).and_then(|of_type| {
      let mut updated_base_field = base_field;
      let resolver = Lambda::context_path(path.clone());
      if has_index {
        updated_base_field.of_type = Type::NamedType { name: of_type.name().to_string(), non_null: false }
      } else {
        updated_base_field.of_type = of_type;
      }

      updated_base_field = updated_base_field.resolver_or_default(resolver, |r| r.to_input_path(path.clone()));
      Valid::succeed(updated_base_field)
    });
  }
  Valid::succeed(base_field)
}
fn to_args(field: &config::Field) -> Valid<Vec<InputFieldDefinition>, String> {
  // TODO! assert type name
  Valid::from_iter(field.args.iter(), |(name, arg)| {
    Valid::succeed(InputFieldDefinition {
      name: name.clone(),
      description: arg.doc.clone(),
      of_type: to_type(&arg.type_of, arg.list, arg.required, false),
      default_value: arg.default_value.clone(),
    })
  })
}
pub fn to_json_schema_for_field(field: &Field, config: &Config) -> JsonSchema {
  to_json_schema(&field.type_of, field.required, field.list, config)
}
pub fn to_json_schema_for_args(args: &BTreeMap<String, Arg>, config: &Config) -> JsonSchema {
  let mut schema_fields = HashMap::new();
  for (name, arg) in args.iter() {
    schema_fields.insert(
      name.clone(),
      to_json_schema(&arg.type_of, arg.required, arg.list, config),
    );
  }
  JsonSchema::Obj(schema_fields)
}
pub fn to_json_schema(type_of: &str, required: bool, list: bool, config: &Config) -> JsonSchema {
  let type_ = config.find_type(type_of);
  let schema = match type_ {
    Some(type_) => {
      let mut schema_fields = HashMap::new();
      for (name, field) in type_.fields.iter() {
        if field.unsafe_operation.is_none() && field.http.is_none() {
          schema_fields.insert(name.clone(), to_json_schema_for_field(field, config));
        }
      }
      JsonSchema::Obj(schema_fields)
    }
    None => match type_of {
      "String" => JsonSchema::Str {},
      "Int" => JsonSchema::Num {},
      "Boolean" => JsonSchema::Bool {},
      "JSON" => JsonSchema::Obj(HashMap::new()),
      _ => JsonSchema::Str {},
    },
  };

  if !required {
    if list {
      JsonSchema::Opt(Box::new(JsonSchema::Arr(Box::new(schema))))
    } else {
      JsonSchema::Opt(Box::new(schema))
    }
  } else if list {
    JsonSchema::Arr(Box::new(schema))
  } else {
    schema
  }
}

impl TryFrom<&Config> for Blueprint {
  type Error = ValidationError<String>;

  fn try_from(config: &Config) -> Result<Self, Self::Error> {
    config_blueprint(config).to_result()
  }
}
