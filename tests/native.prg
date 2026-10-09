#include "fileio.ch"

MEMVAR server
STATIC s_cWritten := "", s_nFallback := 0, s_nChecks := 0

PROCEDURE Main()
   LOCAL hMount := { "/hello" => {| cPath | Legacy( cPath ) }, "/other" => {|| "other" } }
   LOCAL aOwners := { { "hello", "/hello", { "GET", "POST" } } }
   LOCAL hSelected, cError, cMode, nBefore, nRepeat, nStart, nAttributes
   LOCAL nPid, aProcess
   PRIVATE server := { "SCRIPT_NAME" => "/hello", "REQUEST_METHOD" => "GET" }
   ErrorBlock( {| oError | QOut( oError:Description, oError:Operation ), ErrorLevel( 1 ), __Quit() } )

   Check( SliceMounts( hMount, {}, "", @cError ) != NIL, "empty registry" )
   Check( Eval( SliceMounts( hMount, aOwners, "", @cError )[ "/hello" ], "" ) == "legacy", "disabled" )
   Check( SliceMounts( hMount, {}, "hello", @cError ) == NIL, "unregistered activation" )
   Check( SliceMounts( hMount, aOwners, "../hello", @cError ) == NIL, "unsafe activation" )
   Check( SliceMounts( hMount, aOwners, "unknown", @cError ) == NIL, "unknown activation" )
   Check( SliceMounts( hMount, { aOwners[ 1 ], aOwners[ 1 ] }, "", @cError ) == NIL, "duplicate" )
   Check( SliceMounts( hMount, { { "bad-id", "/hello", { "GET", "POST" } } }, "", @cError ) == NIL, "invalid id" )
   Check( SliceMounts( hMount, { { "hello", "/hello/", { "GET", "POST" } } }, "", @cError ) == NIL, "wrong route" )
   Check( SliceMounts( hMount, { { "hello", "/hello", { "GET" } } }, "", @cError ) == NIL, "wrong methods" )
   Check( SliceMounts( { => }, aOwners, "", @cError ) == NIL, "no callback" )
   Check( SliceMounts( hMount, { NIL }, "", @cError ) == NIL, "malformed record" )

   hSelected := SliceMounts( hMount, aOwners, "hello", @cError )
   Check( hSelected != NIL, "explicit selection" )
   Check( Eval( hSelected[ "/other" ] ) == "other", "other callback unchanged" )
   Check( Eval( hMount[ "/hello" ], "" ) == "legacy", "original map unchanged" )
   s_nFallback := 0
   hb_SetEnv( "ESHOP_TEST_MODE", "" )
   Eval( hSelected[ "/hello" ], "" )
   Check( s_cWritten == "Rust fixture!" .AND. s_nFallback == 0, "selected output" )
   s_cWritten := ""
   server[ "REQUEST_METHOD" ] := "POST"
   Eval( hSelected[ "/hello" ], "" )
   Check( s_cWritten == "Rust fixture!" .AND. s_nFallback == 0, "POST selected" )
   server[ "REQUEST_METHOD" ] := "PUT"
   Check( Eval( hSelected[ "/hello" ], "" ) == "legacy", "method fallback" )
   server[ "REQUEST_METHOD" ] := "GET"
   server[ "SCRIPT_NAME" ] := "/hello/"
   Check( Eval( hSelected[ "/hello" ], "" ) == "legacy", "exact path" )
   server[ "SCRIPT_NAME" ] := "/hello"

   nBefore := Len( Directory( "/proc/self/fd/*", "HSD" ) )
   Check( nBefore >= 3, "descriptor observation available" )
   FOR nRepeat := 1 TO 3
      FOR EACH cMode IN { "partial", "sigkill", "sigterm", "sigsegv", "hang", "stdout", "stderr" }
         hb_SetEnv( "ESHOP_TEST_MODE", cMode )
         s_cWritten := ""
         s_nFallback := 0
         nStart := hb_MilliSeconds()
         Check( Eval( hSelected[ "/hello" ], "" ) == "legacy", cMode + " fallback" )
         Check( s_nFallback == 1 .AND. s_cWritten == "", cMode + " atomic fallback" )
         Check( hb_MilliSeconds() - nStart < 2000, cMode + " bounded time" )
      NEXT
   NEXT
   Check( Len( Directory( "/proc/self/fd/*", "HSD" ) ) == nBefore, "no leaked descriptors" )
   nPid := ProcNumber( "/proc/self/status", "Pid:" )
   Check( nPid > 0, "child observation available" )
   FOR EACH aProcess IN Directory( "/proc/*", "D" )
      IF Val( aProcess[ 1 ] ) > 0
         Check( ProcNumber( "/proc/" + aProcess[ 1 ] + "/status", "PPid:" ) != nPid, "no unreaped children" )
      ENDIF
   NEXT
   hb_SetEnv( "ESHOP_TEST_MODE", "empty" )
   Check( SliceBody( "hello" ) == "", "empty body success" )
   hb_SetEnv( "ESHOP_TEST_MODE", "limit" )
   Check( Len( SliceBody( "hello" ) ) == 65536, "exact output limit" )
   hb_SetEnv( "ESHOP_TEST_MODE", "diagnostic" )
   Check( SliceBody( "hello" ) == "Rust fixture!", "stderr drained privately" )

   FRename( "/opt/eshop-slices/hello", "/opt/eshop-slices/hello-hidden" )
   Check( SliceMounts( hMount, aOwners, "hello", @cError ) == NIL, "missing package startup" )
   s_nFallback := 0
   Check( Eval( hSelected[ "/hello" ], "" ) == "legacy" .AND. s_nFallback == 1, "missing executable fallback" )
   FRename( "/opt/eshop-slices/hello-hidden", "/opt/eshop-slices/hello" )
   hb_SetEnv( "ESHOP_TEST_MODE", "" )
   hb_FGetAttr( "/opt/eshop-slices/hello", @nAttributes )
   hb_FSetAttr( "/opt/eshop-slices/hello", hb_bitAnd( nAttributes, hb_bitNot( HB_FA_XUSR + HB_FA_XGRP + HB_FA_XOTH ) ) )
   Check( SliceMounts( hMount, aOwners, "hello", @cError ) == NIL, "nonexecutable startup" )
   Check( SliceBody( "hello" ) == NIL, "spawn failure" )
   hb_FSetAttr( "/opt/eshop-slices/hello", nAttributes )
   Check( Eval( SliceMounts( hMount, aOwners, "", @cError )[ "/hello" ], "" ) == "legacy", "cleared selection rollback" )
   ? "Native checks passed:", s_nChecks
   RETURN

FUNCTION UWrite( cBody )
   s_cWritten += cBody
   RETURN NIL

STATIC FUNCTION Legacy( cPath )
   HB_SYMBOL_UNUSED( cPath )
   s_nFallback++
   RETURN "legacy"

STATIC PROCEDURE Check( lCondition, cLabel )
   s_nChecks++
   IF ! lCondition
      ? "FAIL:", cLabel
      ErrorLevel( 1 )
      QUIT
   ENDIF
   RETURN

/* Read procfs explicitly: these virtual files report a zero file size. */
STATIC FUNCTION ProcNumber( cFile, cField )
   LOCAL hFile := FOpen( cFile ), cBuffer := Space( 4096 ), nRead, cLine
   IF hFile == -1
      RETURN -1
   ENDIF
   nRead := FRead( hFile, @cBuffer, Len( cBuffer ) )
   FClose( hFile )
   FOR EACH cLine IN hb_ATokens( Left( cBuffer, nRead ), Chr( 10 ) )
      IF Left( cLine, Len( cField ) ) == cField
         RETURN Val( StrTran( SubStr( cLine, Len( cField ) + 1 ), Chr( 9 ), " " ) )
      ENDIF
   NEXT
   RETURN -1
