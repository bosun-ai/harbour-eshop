#define SLICE_DEADLINE_MS 1000
#define SLICE_OUTPUT_LIMIT 65536
#define SLICE_STDERR_LIMIT 65536

/* Fixed packaging convention; IDs are checked by the selector before invocation. */
FUNCTION SliceExecutable( cId )
   RETURN "/opt/eshop-slices/" + cId

/* Entire body or NIL: callers may replay only the original stateless callback. */
FUNCTION SliceBody( cId )
   LOCAL hProcess, hInput := -1, hOutput := -1, hError := -1
   LOCAL cInvocation := "ESHOP-BODY/1" + Chr( 10 )
   LOCAL cBody := "", cBuffer := Space( 4096 ), nRead, nErrors := 0
   LOCAL nExit := -1, lValid, nDeadline := hb_MilliSeconds() + SLICE_DEADLINE_MS

   hProcess := hb_processOpen( SliceExecutable( cId ), @hInput, @hOutput, @hError )
   IF hProcess == -1
      RETURN NIL
   ENDIF
   lValid := hb_PWrite( hInput, cInvocation, Len( cInvocation ), 0 ) == Len( cInvocation )
   FClose( hInput )

   DO WHILE lValid .AND. ( nExit == -1 .OR. hOutput != -1 .OR. hError != -1 )
      IF hb_MilliSeconds() >= nDeadline
         lValid := .F.
         EXIT
      ENDIF
      IF hOutput != -1
         nRead := hb_PRead( hOutput, @cBuffer, Len( cBuffer ), 0 )
         IF nRead == -1
            FClose( hOutput )
            hOutput := -1
         ELSEIF nRead > 0
            IF Len( cBody ) + nRead > SLICE_OUTPUT_LIMIT
               lValid := .F.
            ELSE
               cBody += Left( cBuffer, nRead )
            ENDIF
         ENDIF
      ENDIF
      IF hError != -1
         nRead := hb_PRead( hError, @cBuffer, Len( cBuffer ), 0 )
         IF nRead == -1
            FClose( hError )
            hError := -1
         ELSEIF nRead > 0
            nErrors += nRead
            lValid := lValid .AND. nErrors <= SLICE_STDERR_LIMIT
         ENDIF
      ENDIF
      IF nExit == -1
         nExit := hb_processValue( hProcess, .F. )
      ENDIF
      IF nExit < -1 .OR. nExit > 0
         lValid := .F.
      ENDIF
      IF nExit == -1 .OR. hOutput != -1 .OR. hError != -1
         hb_idleSleep( 0.001 )
      ENDIF
   ENDDO

   IF hOutput != -1
      FClose( hOutput )
   ENDIF
   IF hError != -1
      FClose( hError )
   ENDIF
   IF nExit == -1
      hb_processClose( hProcess, .F. )
      hb_processValue( hProcess, .T. )
   ENDIF
   IF lValid .AND. nExit == 0
      RETURN cBody
   ENDIF
   RETURN NIL
