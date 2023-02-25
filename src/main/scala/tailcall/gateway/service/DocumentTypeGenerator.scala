package tailcall.gateway.service

import caliban.introspection.adt.__Type
import caliban.parsing.SourceMapper
import caliban.parsing.adt.Definition.TypeSystemDefinition.TypeDefinition.{FieldDefinition, InputValueDefinition}
import caliban.parsing.adt.{Definition => CalibanDefinition, Document => CalibanDocument, Type => CalibanType}
import caliban.tools.RemoteSchema.parseRemoteSchema
import tailcall.gateway.ast.Document
import zio.{ZIO, ZLayer}

trait DocumentTypeGenerator  {
  def __type(graph: Document): Option[__Type]
}
object DocumentTypeGenerator {
  def __type(document: Document): ZIO[DocumentTypeGenerator, Nothing, Option[__Type]] =
    ZIO.serviceWith[DocumentTypeGenerator](_.__type(document))
  def live: ZLayer[Any, Nothing, DocumentTypeGenerator]                               = ZLayer.succeed(new Live())

  final class Live extends DocumentTypeGenerator {
    private def toCalibanDocument(document: Document): CalibanDocument = {
      CalibanDocument(
        document.definition.map {
          case Document.Definition.ObjectTypeDefinition(name, fields)      => CalibanDefinition.TypeSystemDefinition
              .TypeDefinition.ObjectTypeDefinition(None, name, Nil, Nil, fields.map(toCalibanField))
          case Document.Definition.InputObjectTypeDefinition(name, fields) => CalibanDefinition.TypeSystemDefinition
              .TypeDefinition.InputObjectTypeDefinition(None, name, Nil, fields.map(toCalibanInputValue))
          // case ivdt: Document.Definition.InputValueDefinition              => toCalibanInputValue(ivdt)
          // case fdt: Document.Definition.FieldDefinition                    => toCalibanField(fdt)
        },
        SourceMapper.empty
      )
    }

    private def toCalibanField(field: Document.Definition.FieldDefinition): FieldDefinition =
      FieldDefinition(None, field.name, field.args.map(toCalibanInputValue), toCalibanType(field.ofType), Nil)

    private def toCalibanInputValue(inputValue: Document.Definition.InputValueDefinition): InputValueDefinition =
      CalibanDefinition.TypeSystemDefinition.TypeDefinition
        .InputValueDefinition(None, inputValue.name, toCalibanType(inputValue.ofType), None, Nil)

    private def toCalibanType(tpe: Document.Type): CalibanType =
      tpe match {
        case Document.Type.NamedType(name, nonNull)  => CalibanType.NamedType(name, nonNull)
        case Document.Type.ListType(ofType, nonNull) => CalibanType.ListType(toCalibanType(ofType), nonNull)
      }

    override def __type(doc: Document): Option[__Type] = parseRemoteSchema(toCalibanDocument(doc)).map(_.queryType)
  }
}
