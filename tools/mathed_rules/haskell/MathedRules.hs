{-# LANGUAGE QuasiQuotes #-}
{-# LANGUAGE DataKinds #-}

-- | Authoring-time Egison pattern engine for mathed templates (T5).
--
-- A direct sibling of australVM's fock_match (same TH quasiquoter
-- style, same GHC 9.10.3 + sweet-egison env from the unfer flake):
-- consumes `{op, body}` JSON on stdin, writes `{markup}` JSON on
-- stdout. The binary is a dev-machine convenience — `--render-typst`
-- degrades to the identity path when it is absent.
--
-- Two v1 jobs:
--
--   op = rewrite : body is a comma-separated token list
--                  (`a†, a, b`; `†` marks the adjoint). Finds the
--                  first adjacent dagger-then-plain pair and, when
--                  the names match, contracts it to a `⟨name⟩`
--                  marker, recursing (fock_match's `normalOrder`
--                  structure). Output: the remaining tokens joined
--                  by spaces.
--
--   op = select  : body is `name:value;name:value;…` (the Rust side
--                  pre-slices DocumentContext.statements — the full
--                  ctx is not shipped to the binary in v1, an
--                  as-built deviation from the {ctx, body} sketch).
--                  Selects every statement whose value equals
--                  `self(<name>)` — a non-linear pattern binding the
--                  statement's own name (twinPrimes' `#(p + 2)`
--                  style). Output: the selected names joined by `;`.
module Main where

import Control.Egison
import Control.Egison.Matcher.Collection
import Control.Monad.Search (dfs)
import Data.Char (chr, isSpace, ord)
import Data.List (intercalate, isPrefixOf, isSuffixOf)

-- ── job 1: contraction-summary rewrite ──────────────────────────

type Token = (String, Bool) -- (name, dagger?)
type Tagged = (Int, Token)

-- First adjacent (x†, y) pair with x == y, if any. The `Eql`
-- matcher on the second component follows fock_match's shape;
-- name equality is checked after the match (as fock_match does).
findContraction :: [Tagged] -> Maybe (Int, String)
findContraction ts =
  case matchAll dfs ts (List (Something, (Something, Eql)))
         [[mc| _ ++ ($i, ($n1, #True)) : (_, ($n2, #False)) : _ -> (i, n1, n2) |]] of
    (i, n1, n2) : _ | n1 == n2 -> Just (i, n1)
    _                          -> Nothing

-- Replace the pair starting at index i with a single ⟨name⟩ token.
contractAt :: Int -> String -> [Tagged] -> [Tagged]
contractAt i n ts =
  let (a, pair) = splitAt i ts
      rest = drop 2 pair
  in a ++ [(i, ("⟨" ++ n ++ "⟩", False))] ++ rest

rewriteTokens :: [Tagged] -> [Tagged]
rewriteTokens ts =
  case findContraction ts of
    Nothing   -> ts
    Just (i, n) -> rewriteTokens (contractAt i n ts)

rewriteBody :: String -> String
rewriteBody body =
  unwords [name | (_, (name, _)) <- rewriteTokens (parseTokens body)]

parseTokens :: String -> [Tagged]
parseTokens body = zip [0 ..] (map parseToken (splitOn ',' body))

parseToken :: String -> Token
parseToken s0 =
  let s = trim s0
  in if "†" `isSuffixOf` s
       then (trim (init s), True)
       else (s, False)

-- ── job 2: Eql-bound fragment selection ─────────────────────────
-- Input: `name:value;name:value;…`. Selects statements whose value
-- is exactly `self(<name>)`, binding the statement's own name — the
-- XSLT `<xsl:template match>` role over DocumentContext statements.

type Statement = (Int, (String, String)) -- (index, (name, value))

parseStatements :: String -> [Statement]
parseStatements body = zip [0 ..] (map parsePair (splitOn ';' body))

parsePair :: String -> (String, String)
parsePair s =
  let (n, v) = break (== ':') s
  in (trim n, trim (drop 1 v))

selectSelfRefs :: [Statement] -> [(Int, String)]
selectSelfRefs stmts =
  matchAll dfs stmts (List (Something, (Something, Eql)))
    [[mc| _ ++ ($i, ($n, #("self(" ++ n ++ ")"))) : _ -> (i, n) |]]

-- ── JSON I/O (minimal: {op, body} in, {markup} out) ────────────

main :: IO ()
main = do
  input <- getContents
  let out = case (extractField "op" input, extractField "body" input) of
              (Just "rewrite", Just b) -> rewriteBody b
              (Just "select",  Just b) -> intercalate ";" (map snd (selectSelfRefs (parseStatements b)))
              _                        -> ""
  putStrLn ("{\"markup\":" ++ jsonEscape out ++ "}")

-- Value of the first JSON string whose key is `key` (searched as
-- `"key":"`), JSON-unescaped.
extractField :: String -> String -> Maybe String
extractField key s =
  let needle = '"' : key ++ "\":\""
  in case findSub needle s of
       Nothing   -> Nothing
       Just rest -> Just (unescapeJson (takeWhileJson rest))

findSub :: String -> String -> Maybe String
findSub needle hay
  | needle `isPrefixOf` hay = Just (drop (length needle) hay)
  | otherwise = case hay of
      _ : rest -> findSub needle rest
      []       -> Nothing

-- Take chars until an unescaped closing quote (escapes kept intact
-- for the decoder).
takeWhileJson :: String -> String
takeWhileJson ('\\' : c : rest) = '\\' : c : takeWhileJson rest
takeWhileJson ('"' : _)         = ""
takeWhileJson (c : rest)        = c : takeWhileJson rest
takeWhileJson []                = []

-- Minimal JSON string unescape (quotes, backslash, n, t, uXXXX).
unescapeJson :: String -> String
unescapeJson ('\\' : '"' : rest)          = '"' : unescapeJson rest
unescapeJson ('\\' : '\\' : rest)         = '\\' : unescapeJson rest
unescapeJson ('\\' : 'n' : rest)          = '\n' : unescapeJson rest
unescapeJson ('\\' : 't' : rest)          = '\t' : unescapeJson rest
unescapeJson ('\\' : 'u' : d1 : d2 : d3 : d4 : rest) =
  chr (hex d1 * 4096 + hex d2 * 256 + hex d3 * 16 + hex d4) : unescapeJson rest
  where hex n
          | n >= '0' && n <= '9' = ord n - ord '0'
          | n >= 'a' && n <= 'f' = ord n - ord 'a' + 10
          | n >= 'A' && n <= 'F' = ord n - ord 'A' + 10
          | otherwise            = 0
unescapeJson (c : rest) = c : unescapeJson rest
unescapeJson []         = []

-- Emit a JSON string.
jsonEscape :: String -> String
jsonEscape = concatMap esc
  where
    esc '"'  = "\\\""
    esc '\\' = "\\\\"
    esc '\n' = "\\n"
    esc c    = [c]

-- ── small helpers ───────────────────────────────────────────────

splitOn :: Char -> String -> [String]
splitOn c s = case break (== c) s of
  (a, [])     -> [a]
  (a, _ : rest) -> a : splitOn c rest

trim :: String -> String
trim = f . f
  where f = reverse . dropWhile isSpace