/* Test build replaces only the production-empty ownership module. */
FUNCTION SliceOwnership()
   RETURN { { "hello", "/hello", { "GET", "POST" } } }
