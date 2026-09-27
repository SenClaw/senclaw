export const metadata = {
  title: 'Google Calendar Space App'
};

export default function RootLayout({ children }) {
  return (
    <html lang="en">
      <body>{children}</body>
    </html>
  );
}
