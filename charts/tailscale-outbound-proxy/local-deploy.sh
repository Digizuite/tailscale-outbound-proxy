helm upgrade tailscale-outbound-proxy . -n ts-out-proxy --set operator.enable=true --install --create-namespace --set tailscale.client_id="$TAILSCALE_CLIENT_ID" --set tailscale.client_secret="$TAILSCALE_CLIENT_SECRET"
