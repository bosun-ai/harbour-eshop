#include "fileio.ch"

MEMVAR server

/* Returns NIL and an error for invalid startup configuration; never auto-enables. */
FUNCTION SliceMounts( hMount, aOwners, cEnabled, cError )
   LOCAL hIds := { => }, hRoutes := { => }, aOwner, cId, bLegacy, nAttributes

   cError := ""
   FOR EACH aOwner IN aOwners
      IF ! HB_ISARRAY( aOwner ) .OR. Len( aOwner ) != 3
         cError := "Invalid slice ownership record"
         RETURN NIL
      ENDIF
      cId := aOwner[ 1 ]
      IF ! SliceValidId( cId )
         cError := "Invalid slice ID"
         RETURN NIL
      ENDIF
      /* This bootstrap supports only the stateless /hello candidate. */
      IF cId != "hello" .OR. aOwner[ 2 ] != "/hello" .OR. ;
            ! HB_ISARRAY( aOwner[ 3 ] ) .OR. Len( aOwner[ 3 ] ) != 2 .OR. ;
            aOwner[ 3 ][ 1 ] != "GET" .OR. aOwner[ 3 ][ 2 ] != "POST"
         cError := "Unsupported slice ownership"
         RETURN NIL
      ENDIF
      IF hb_HHasKey( hIds, cId ) .OR. hb_HHasKey( hRoutes, aOwner[ 2 ] )
         cError := "Conflicting slice ownership"
         RETURN NIL
      ENDIF
      IF ! hb_HHasKey( hMount, aOwner[ 2 ] )
         cError := "Slice route has no legacy callback"
         RETURN NIL
      ENDIF
      hIds[ cId ] := aOwner
      hRoutes[ aOwner[ 2 ] ] := .T.
   NEXT

   IF cEnabled == ""
      RETURN hMount
   ENDIF
   IF ! SliceValidId( cEnabled ) .OR. ! hb_HHasKey( hIds, cEnabled )
      cError := "Unknown enabled slice"
      RETURN NIL
   ENDIF
   IF ! hb_FileExists( SliceExecutable( cEnabled ) ) .OR. ;
         ! hb_FGetAttr( SliceExecutable( cEnabled ), @nAttributes ) .OR. ;
         hb_bitAnd( nAttributes, HB_FA_XUSR + HB_FA_XGRP + HB_FA_XOTH ) == 0
      cError := "Enabled slice executable is not packaged"
      RETURN NIL
   ENDIF

   hMount := hb_HClone( hMount )
   aOwner := hIds[ cEnabled ]
   bLegacy := hMount[ aOwner[ 2 ] ]
   hMount[ aOwner[ 2 ] ] := {| cPath | SliceDispatch( aOwner, bLegacy, cPath ) }
   RETURN hMount

STATIC FUNCTION SliceValidId( cId )
   LOCAL nIndex, cCharacter
   IF ! HB_ISSTRING( cId ) .OR. Len( cId ) < 1 .OR. Len( cId ) > 32
      RETURN .F.
   ENDIF
   FOR nIndex := 1 TO Len( cId )
      cCharacter := SubStr( cId, nIndex, 1 )
      IF ! cCharacter $ "abcdefghijklmnopqrstuvwxyz0123456789_"
         RETURN .F.
      ENDIF
   NEXT
   RETURN .T.

/* No request data crosses the process boundary, including POST bodies. */
FUNCTION SliceDispatch( aOwner, bLegacy, cPath )
   LOCAL cBody
   IF server[ "SCRIPT_NAME" ] == aOwner[ 2 ] .AND. ;
         AScan( aOwner[ 3 ], {| cMethod | cMethod == server[ "REQUEST_METHOD" ] } ) > 0
      cBody := SliceBody( aOwner[ 1 ] )
      IF cBody != NIL
         UWrite( cBody )
         RETURN NIL
      ENDIF
   ENDIF
   RETURN Eval( bLegacy, cPath )
