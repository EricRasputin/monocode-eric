import { Toaster } from "sonner";
import { useColorScheme } from "../hooks/useColorScheme";
import { LAYER } from "../lib/layers";
import { X } from "./icons";

export function AppToaster() {
  const theme = useColorScheme();

  return (
    <Toaster
      className="monocode-toaster"
      theme={theme}
      position="bottom-right"
      offset={16}
      mobileOffset={16}
      gap={8}
      duration={6500}
      closeButton
      style={{ zIndex: LAYER.toast }}
      icons={{ close: <X className="size-3" /> }}
      toastOptions={{ closeButtonAriaLabel: "Dismiss notification" }}
    />
  );
}
