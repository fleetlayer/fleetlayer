#!/bin/sh
# A stand-in for Playwright MCP in browser.rs's tests: logs how it's started and what it's asked (to LOG), and answers
# like Playwright MCP 0.0.86 does.
echo "args: $*" >> LOG
echo "token: $PLAYWRIGHT_MCP_EXTENSION_TOKEN" >> LOG
while read -r line; do
  id=$(echo "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  echo "$line" | grep -o '"name":"[a-z_]*"\|"arguments":{[^}]*}' >> LOG
  case "$line" in
    *'"initialize"'*) printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{}}" ;;
    *tools/list*) printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"tools\":[{\"name\":\"browser_cookie_list\"}]}}" ;;
    *browser_cookie_list*) printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"### Result\\nauth_token=abc (domain: .x.com, path: /)\\nother=no (domain: .notx.com, path: /)\"}]}}" ;;
    *'"id"'*) printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"content\":[]}}" ;;
  esac
done
